use diffy::DiffOptions;
use euler_event::{object, JsonObject};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, DirEntry};
use std::io::{Read as _, Seek as _};
use std::path::{Path, PathBuf};
use thiserror::Error;

use crate::sandbox::FrozenReadOnlySurface;

pub const MAX_FILE_DIFF_BYTES: usize = 64 * 1024;
pub const MAX_WORKSPACE_SNAPSHOT_FILES: usize = 4_096;
pub const MAX_WORKSPACE_SNAPSHOT_FILE_BYTES: usize = 256 * 1024;
pub const MAX_WORKSPACE_SNAPSHOT_TOTAL_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_WORKSPACE_OPAQUE_ENTRIES: usize = 150_000;
pub const MAX_WORKSPACE_OPAQUE_FILE_HASH_BYTES: usize = 64 * 1024;
pub const MAX_WORKSPACE_OPAQUE_TOTAL_HASH_BYTES: usize = 8 * 1024 * 1024;

const TRUNCATED_MARKER: &str = "\n...[truncated]\n";

pub struct FileDiffSource<'a> {
    pub path: &'a str,
    pub action: &'a str,
    pub before: &'a str,
    pub after: &'a str,
}

pub struct FileDiffProjection {
    pub diff: Option<String>,
    pub truncated: bool,
    pub truncation: &'static str,
    pub omitted_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedFileChange {
    /// Canonical writable root in which `path` is relative. This disambiguates
    /// the same relative path across an explicitly attached multi-root run.
    pub workspace_root: String,
    pub path: String,
    pub file_type: &'static str,
    pub before_file_type: Option<&'static str>,
    pub after_file_type: Option<&'static str>,
    pub action: &'static str,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
    pub before_metadata_sha256: Option<String>,
    pub after_metadata_sha256: Option<String>,
    pub before_byte_len: usize,
    pub after_byte_len: usize,
    pub before_mode: Option<u32>,
    pub after_mode: Option<u32>,
    before_text: Option<String>,
    after_text: Option<String>,
    diff_omitted_reason: Option<&'static str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSnapshot {
    workspace_root: String,
    /// Protected subtrees discovered before an agent command starts. A later
    /// snapshot must reuse this exact set: rediscovering it afterward would
    /// let a command create a lookalike `.worktrees` directory and have its
    /// contents incorrectly skipped as though Bubblewrap had protected it.
    read_only_surfaces: BTreeMap<String, FrozenReadOnlySurface>,
    files: BTreeMap<String, SnapshotEntry>,
    hardlinks: BTreeMap<(u64, u64), HardlinkObservation>,
}

#[derive(Default)]
struct CaptureBudget {
    granular_bytes: usize,
    opaque_entries: usize,
    opaque_hashed_content_bytes: usize,
}

impl CaptureBudget {
    fn remaining_opaque_entries(&self) -> usize {
        MAX_WORKSPACE_OPAQUE_ENTRIES.saturating_sub(self.opaque_entries)
    }

    fn record_opaque_entries(&mut self, count: usize) -> Result<(), WorkspaceSnapshotError> {
        self.opaque_entries = self
            .opaque_entries
            .checked_add(count)
            .filter(|total| *total <= MAX_WORKSPACE_OPAQUE_ENTRIES)
            .ok_or(WorkspaceSnapshotError::OpaqueEntryLimit)?;
        Ok(())
    }
}

/// A single fd-anchored regular-file observation used to preserve provenance
/// when a structured write fails after opening or creating its target.
pub(crate) struct StructuredFileSnapshot {
    workspace_root: String,
    path: String,
    entry: Option<SnapshotEntry>,
}

impl StructuredFileSnapshot {
    pub(crate) fn absent(
        workspace_root: &Path,
        path: &str,
    ) -> Result<Self, WorkspaceSnapshotError> {
        Ok(Self {
            workspace_root: workspace_root
                .to_str()
                .ok_or(WorkspaceSnapshotError::InvalidRoot)?
                .to_owned(),
            path: path.to_owned(),
            entry: None,
        })
    }

    pub(crate) fn capture_open_regular(
        workspace_root: &Path,
        path: &str,
        file: &fs::File,
    ) -> Result<Self, WorkspaceSnapshotError> {
        let mut reader = file
            .try_clone()
            .map_err(|_| WorkspaceSnapshotError::FileRead)?;
        // `try_clone` duplicates the descriptor and therefore shares its open
        // file description (including the seek offset) on Unix. Restore that
        // offset on every path so observation cannot change write semantics.
        let original_position = reader
            .stream_position()
            .map_err(|_| WorkspaceSnapshotError::FileRead)?;
        reader
            .seek(std::io::SeekFrom::Start(0))
            .map_err(|_| WorkspaceSnapshotError::FileRead)?;
        let captured = (|| {
            let metadata = reader
                .metadata()
                .map_err(|_| WorkspaceSnapshotError::MetadataRead)?;
            if !metadata.is_file() {
                return Err(WorkspaceSnapshotError::MetadataRead);
            }
            let mut bytes = Vec::new();
            reader
                .read_to_end(&mut bytes)
                .map_err(|_| WorkspaceSnapshotError::FileRead)?;
            let stable_metadata = reader
                .metadata()
                .map_err(|_| WorkspaceSnapshotError::MetadataRead)?;
            if !stable_metadata.is_file()
                || usize::try_from(stable_metadata.len()).ok() != Some(bytes.len())
                || metadata_fingerprint(&metadata) != metadata_fingerprint(&stable_metadata)
            {
                return Err(WorkspaceSnapshotError::FileRead);
            }
            Ok(Self {
                workspace_root: workspace_root
                    .to_str()
                    .ok_or(WorkspaceSnapshotError::InvalidRoot)?
                    .to_owned(),
                path: path.to_owned(),
                entry: Some(SnapshotEntry::regular(bytes, &stable_metadata)),
            })
        })();
        reader
            .seek(std::io::SeekFrom::Start(original_position))
            .map_err(|_| WorkspaceSnapshotError::FileRead)?;
        captured
    }

    pub(crate) fn change_to(&self, after: &Self) -> Option<ObservedFileChange> {
        if self.workspace_root != after.workspace_root || self.path != after.path {
            return None;
        }
        observed_change(
            self.workspace_root.clone(),
            self.path.clone(),
            self.entry.as_ref(),
            after.entry.as_ref(),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SnapshotEntry {
    kind: SnapshotKind,
    sha256: String,
    metadata_sha256: String,
    byte_len: usize,
    mode: u32,
    text: Option<String>,
    observed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotKind {
    Regular,
    Symlink,
    Directory,
    OpaqueDirectory,
    ReadOnlyDirectory,
    Fifo,
    Socket,
    BlockDevice,
    CharDevice,
    Other,
}

impl SnapshotKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Symlink => "symlink",
            Self::Directory => "directory",
            Self::OpaqueDirectory => "opaque-directory",
            Self::ReadOnlyDirectory => "read-only-directory",
            Self::Fifo => "fifo",
            Self::Socket => "socket",
            Self::BlockDevice => "block-device",
            Self::CharDevice => "char-device",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HardlinkObservation {
    observed_links: u64,
    inode_links: u64,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum WorkspaceSnapshotError {
    #[error("workspace observation could not resolve the writable root")]
    InvalidRoot,
    #[error("workspace observation could not read the complete directory tree")]
    DirectoryRead,
    #[error("workspace observation could not read complete file metadata")]
    MetadataRead,
    #[error("workspace observation exceeded the {MAX_WORKSPACE_SNAPSHOT_FILES}-entry limit")]
    FileLimit,
    #[error(
        "workspace observation exceeded the {MAX_WORKSPACE_SNAPSHOT_TOTAL_BYTES}-byte read limit"
    )]
    TotalBytesLimit,
    #[error("workspace observation could not read a complete file")]
    FileRead,
    #[error(
        "workspace observation exceeded the {MAX_WORKSPACE_OPAQUE_ENTRIES}-entry opaque-surface limit"
    )]
    OpaqueEntryLimit,
    #[error("workspace contains a hard-linked file with an alias outside writable authority")]
    ExternalHardlink,
    #[error(
        "workspace observation could not freeze or preserve a protected subtree's real identity"
    )]
    ProtectedSurfaceIdentity,
}

pub fn file_diff_projection(source: FileDiffSource<'_>) -> FileDiffProjection {
    if let Some(reason) = omission_reason(&source) {
        return omitted(reason);
    }
    let (diff, truncated) = bounded_diff(unified_diff(&source));
    FileDiffProjection {
        diff: Some(diff),
        truncated,
        truncation: if truncated { "tail" } else { "none" },
        omitted_reason: truncated.then(|| format!("diff exceeded {MAX_FILE_DIFF_BYTES} bytes")),
    }
}

pub fn observed_file_diff_projection(change: &ObservedFileChange) -> FileDiffProjection {
    if let Some(reason) = change.diff_omitted_reason {
        return omitted(reason);
    }
    match change.action {
        "add" => observed_diff(&change.path, "add", "", change.after_text.as_deref()),
        "modify" => {
            let Some(before) = change.before_text.as_deref() else {
                return omitted("binary");
            };
            observed_diff(&change.path, "modify", before, change.after_text.as_deref())
        }
        "delete" => omitted("delete-content"),
        _ => omitted("unsupported-action"),
    }
}

pub fn observed_file_change_payload(
    tool_call_id: &str,
    origin: &str,
    change: &ObservedFileChange,
) -> JsonObject {
    object([
        ("tool_call_id", tool_call_id.to_owned().into()),
        ("origin", origin.into()),
        ("action", change.action.into()),
        ("workspace_root", change.workspace_root.clone().into()),
        ("path", change.path.clone().into()),
        ("file_type", change.file_type.into()),
        (
            "before_file_type",
            optional_static_str(change.before_file_type),
        ),
        (
            "after_file_type",
            optional_static_str(change.after_file_type),
        ),
        ("old_path", Value::Null),
        ("before_sha256", optional_string(&change.before_sha256)),
        ("after_sha256", optional_string(&change.after_sha256)),
        (
            "before_metadata_sha256",
            optional_string(&change.before_metadata_sha256),
        ),
        (
            "after_metadata_sha256",
            optional_string(&change.after_metadata_sha256),
        ),
        ("before_byte_len", change.before_byte_len.into()),
        ("after_byte_len", change.after_byte_len.into()),
        ("before_mode", optional_u32(change.before_mode)),
        ("after_mode", optional_u32(change.after_mode)),
        ("diff_redaction", "omitted".into()),
    ])
}

/// Describe a mutation observed after a failed workspace checkpoint restore.
/// A restore is a session control operation, not a model tool call, so its
/// association uses the durable `workspace.restore` event id explicitly.
pub fn observed_workspace_restore_change_payload(
    workspace_restore_id: &str,
    change: &ObservedFileChange,
) -> JsonObject {
    let mut payload = observed_file_change_payload("", "workspace_restore", change);
    payload.remove("tool_call_id");
    payload.insert(
        "workspace_restore_id".to_owned(),
        workspace_restore_id.to_owned().into(),
    );
    payload
}

pub fn observed_file_diff_payload(
    tool_call_id: &str,
    file_change_id: &str,
    origin: &str,
    change: &ObservedFileChange,
) -> JsonObject {
    let projection = observed_file_diff_projection(change);
    object([
        ("tool_call_id", tool_call_id.to_owned().into()),
        ("file_change_id", file_change_id.to_owned().into()),
        ("workspace_root", change.workspace_root.clone().into()),
        ("path", change.path.clone().into()),
        ("file_type", change.file_type.into()),
        (
            "before_file_type",
            optional_static_str(change.before_file_type),
        ),
        (
            "after_file_type",
            optional_static_str(change.after_file_type),
        ),
        ("old_path", Value::Null),
        ("action", change.action.into()),
        ("origin", origin.into()),
        ("before_mode", optional_u32(change.before_mode)),
        ("after_mode", optional_u32(change.after_mode)),
        ("before_sha256", optional_string(&change.before_sha256)),
        ("after_sha256", optional_string(&change.after_sha256)),
        (
            "before_metadata_sha256",
            optional_string(&change.before_metadata_sha256),
        ),
        (
            "after_metadata_sha256",
            optional_string(&change.after_metadata_sha256),
        ),
        ("before_byte_len", change.before_byte_len.into()),
        ("after_byte_len", change.after_byte_len.into()),
        (
            "diff",
            projection
                .diff
                .map_or(Value::Null, std::convert::Into::into),
        ),
        ("truncated", projection.truncated.into()),
        ("truncation", projection.truncation.into()),
        (
            "omitted_reason",
            projection
                .omitted_reason
                .map_or(Value::Null, std::convert::Into::into),
        ),
    ])
}

/// Display projection paired with
/// [`observed_workspace_restore_change_payload`].
pub fn observed_workspace_restore_diff_payload(
    workspace_restore_id: &str,
    file_change_id: &str,
    change: &ObservedFileChange,
) -> JsonObject {
    let mut payload = observed_file_diff_payload("", file_change_id, "workspace_restore", change);
    payload.remove("tool_call_id");
    payload.insert(
        "workspace_restore_id".to_owned(),
        workspace_restore_id.to_owned().into(),
    );
    payload
}

pub fn capture_workspace_snapshot(
    root: &Path,
) -> Result<WorkspaceSnapshot, WorkspaceSnapshotError> {
    WorkspaceSnapshot::capture(root)
}

/// Capture the post-command state using the protected-surface identity from
/// the corresponding pre-command snapshot.
pub fn recapture_workspace_snapshot(
    before: &WorkspaceSnapshot,
) -> Result<WorkspaceSnapshot, WorkspaceSnapshotError> {
    WorkspaceSnapshot::capture_with_read_only_surfaces(
        Path::new(&before.workspace_root),
        before.read_only_surfaces.clone(),
    )
}

/// Reject snapshots whose writable inodes have aliases outside the complete
/// attached writable-root set. A read-only bind mount cannot constrain writes
/// through a hardlink reached from a writable mount, so this is part of the
/// authority boundary rather than merely provenance bookkeeping.
pub fn validate_workspace_snapshot_hardlinks(
    snapshots: &[WorkspaceSnapshot],
) -> Result<(), WorkspaceSnapshotError> {
    let mut aggregate = BTreeMap::<(u64, u64), HardlinkObservation>::new();
    for snapshot in snapshots {
        for (identity, observed) in &snapshot.hardlinks {
            let total = aggregate.entry(*identity).or_insert(HardlinkObservation {
                observed_links: 0,
                inode_links: observed.inode_links,
            });
            total.observed_links = total.observed_links.saturating_add(observed.observed_links);
            total.inode_links = total.inode_links.max(observed.inode_links);
        }
    }
    if aggregate
        .values()
        .any(|links| links.inode_links > links.observed_links)
    {
        return Err(WorkspaceSnapshotError::ExternalHardlink);
    }
    Ok(())
}

impl WorkspaceSnapshot {
    pub(crate) fn frozen_read_only_surfaces(&self) -> impl Iterator<Item = &FrozenReadOnlySurface> {
        self.read_only_surfaces.values()
    }

    pub fn changes_to(&self, after: &Self) -> Vec<ObservedFileChange> {
        if self.workspace_root != after.workspace_root {
            return Vec::new();
        }
        let paths = self
            .files
            .keys()
            .chain(after.files.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        paths
            .into_iter()
            .filter_map(|path| {
                let before_file = self.files.get(&path);
                let after_file = after.files.get(&path);
                observed_change(self.workspace_root.clone(), path, before_file, after_file)
            })
            .collect()
    }

    fn capture(root: &Path) -> Result<Self, WorkspaceSnapshotError> {
        let read_only_surfaces = discover_read_only_surfaces(root)?;
        Self::capture_with_read_only_surfaces(root, read_only_surfaces)
    }

    fn capture_with_read_only_surfaces(
        root: &Path,
        read_only_surfaces: BTreeMap<String, FrozenReadOnlySurface>,
    ) -> Result<Self, WorkspaceSnapshotError> {
        let requested_root = root.to_path_buf();
        let root = requested_root
            .canonicalize()
            .map_err(|_| WorkspaceSnapshotError::InvalidRoot)?;
        if root != requested_root
            || !fs::symlink_metadata(&root)
                .map_err(|_| WorkspaceSnapshotError::InvalidRoot)?
                .file_type()
                .is_dir()
        {
            return Err(WorkspaceSnapshotError::InvalidRoot);
        }
        let workspace_root = root
            .to_str()
            .ok_or(WorkspaceSnapshotError::InvalidRoot)?
            .to_owned();
        validate_read_only_surfaces(&root, &read_only_surfaces)?;
        let mut snapshot = Self {
            workspace_root,
            read_only_surfaces,
            files: BTreeMap::new(),
            hardlinks: BTreeMap::new(),
        };
        let root_metadata =
            fs::symlink_metadata(&root).map_err(|_| WorkspaceSnapshotError::MetadataRead)?;
        snapshot.record_metadata_entry(
            ".".to_owned(),
            SnapshotEntry::metadata(
                SnapshotKind::Directory,
                hash_bytes(b"directory"),
                &root_metadata,
                0,
            ),
        )?;
        let mut stack = vec![(root.clone(), String::new())];
        let mut budget = CaptureBudget::default();
        while let Some((dir, relative_dir)) = stack.pop() {
            let remaining = MAX_WORKSPACE_SNAPSHOT_FILES.saturating_sub(snapshot.files.len());
            let entries =
                read_sorted_dir_bounded(&dir, remaining, WorkspaceSnapshotError::FileLimit)?;
            snapshot.record_entries(entries, &relative_dir, &mut stack, &mut budget)?;
        }
        validate_read_only_surfaces(&root, &snapshot.read_only_surfaces)?;
        Ok(snapshot)
    }

    fn record_entries(
        &mut self,
        entries: Vec<DirEntry>,
        relative_dir: &str,
        stack: &mut Vec<(PathBuf, String)>,
        budget: &mut CaptureBudget,
    ) -> Result<(), WorkspaceSnapshotError> {
        for entry in entries {
            self.record_entry(entry, relative_dir, stack, budget)?;
        }
        Ok(())
    }

    fn record_entry(
        &mut self,
        entry: DirEntry,
        relative_dir: &str,
        stack: &mut Vec<(PathBuf, String)>,
        budget: &mut CaptureBudget,
    ) -> Result<(), WorkspaceSnapshotError> {
        let name = entry
            .file_name()
            .to_str()
            .map(str::to_owned)
            .ok_or(WorkspaceSnapshotError::MetadataRead)?;
        let path = relative_path(relative_dir, &name);
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|_| WorkspaceSnapshotError::MetadataRead)?;
        if metadata.file_type().is_symlink() {
            let target =
                fs::read_link(entry.path()).map_err(|_| WorkspaceSnapshotError::FileRead)?;
            let target = path_bytes(target.as_os_str());
            return self.record_metadata_entry(
                path,
                SnapshotEntry::metadata(
                    SnapshotKind::Symlink,
                    hash_bytes(&target),
                    &metadata,
                    target.len(),
                ),
            );
        }
        if metadata.is_dir() {
            if relative_dir.is_empty() && self.read_only_surfaces.contains_key(&path) {
                return self.record_metadata_entry(
                    path,
                    SnapshotEntry::metadata(
                        SnapshotKind::ReadOnlyDirectory,
                        hash_bytes(b"read-only-directory"),
                        &metadata,
                        0,
                    ),
                );
            }
            if ignored_dir(OsStr::new(&name)) {
                let digest = self.opaque_directory_digest(&entry.path(), budget)?;
                return self.record_metadata_entry(
                    path,
                    SnapshotEntry::metadata(SnapshotKind::OpaqueDirectory, digest, &metadata, 0),
                );
            }
            self.record_metadata_entry(
                path.clone(),
                SnapshotEntry::metadata(
                    SnapshotKind::Directory,
                    hash_bytes(b"directory"),
                    &metadata,
                    0,
                ),
            )?;
            stack.push((entry.path(), path));
            return Ok(());
        }
        if metadata.is_file() {
            return self.record_file(entry.path(), path, &metadata, budget);
        }
        let kind = special_file_kind(&metadata);
        self.record_metadata_entry(
            path,
            SnapshotEntry::metadata(
                kind,
                hash_bytes(kind.as_str().as_bytes()),
                &metadata,
                usize::try_from(metadata.len()).unwrap_or(usize::MAX),
            ),
        )
    }

    fn record_file(
        &mut self,
        path: PathBuf,
        relative: String,
        metadata: &fs::Metadata,
        budget: &mut CaptureBudget,
    ) -> Result<(), WorkspaceSnapshotError> {
        if self.files.len() >= MAX_WORKSPACE_SNAPSHOT_FILES {
            return Err(WorkspaceSnapshotError::FileLimit);
        }
        let byte_len =
            usize::try_from(metadata.len()).map_err(|_| WorkspaceSnapshotError::TotalBytesLimit)?;
        let next_total = budget
            .granular_bytes
            .checked_add(byte_len)
            .ok_or(WorkspaceSnapshotError::TotalBytesLimit)?;
        if next_total > MAX_WORKSPACE_SNAPSHOT_TOTAL_BYTES {
            return Err(WorkspaceSnapshotError::TotalBytesLimit);
        }
        let bytes = fs::read(&path).map_err(|_| WorkspaceSnapshotError::FileRead)?;
        if bytes.len() != byte_len {
            return Err(WorkspaceSnapshotError::FileRead);
        }
        let stable_metadata =
            fs::symlink_metadata(&path).map_err(|_| WorkspaceSnapshotError::MetadataRead)?;
        if !stable_metadata.is_file()
            || metadata_fingerprint(metadata) != metadata_fingerprint(&stable_metadata)
        {
            return Err(WorkspaceSnapshotError::FileRead);
        }
        budget.granular_bytes = next_total;
        self.record_hardlink(&stable_metadata);
        self.files
            .insert(relative, SnapshotEntry::regular(bytes, &stable_metadata));
        Ok(())
    }

    fn record_metadata_entry(
        &mut self,
        relative: String,
        entry: SnapshotEntry,
    ) -> Result<(), WorkspaceSnapshotError> {
        if self.files.len() >= MAX_WORKSPACE_SNAPSHOT_FILES {
            return Err(WorkspaceSnapshotError::FileLimit);
        }
        self.files.insert(relative, entry);
        Ok(())
    }

    fn record_hardlink(&mut self, metadata: &fs::Metadata) {
        let Some((device, inode, links)) = hardlink_identity(metadata) else {
            return;
        };
        let observation = self
            .hardlinks
            .entry((device, inode))
            .or_insert(HardlinkObservation {
                observed_links: 0,
                inode_links: links,
            });
        observation.observed_links = observation.observed_links.saturating_add(1);
        observation.inode_links = observation.inode_links.max(links);
    }

    fn opaque_directory_digest(
        &mut self,
        root: &Path,
        budget: &mut CaptureBudget,
    ) -> Result<String, WorkspaceSnapshotError> {
        let mut hasher = Sha256::new();
        let mut stack = vec![(root.to_path_buf(), String::new())];
        while let Some((directory, relative_dir)) = stack.pop() {
            let remaining = budget.remaining_opaque_entries();
            let entries = read_sorted_dir_bounded(
                &directory,
                remaining,
                WorkspaceSnapshotError::OpaqueEntryLimit,
            )?;
            budget.record_opaque_entries(entries.len())?;
            for entry in entries {
                let name = entry
                    .file_name()
                    .to_str()
                    .map(str::to_owned)
                    .ok_or(WorkspaceSnapshotError::MetadataRead)?;
                let relative = relative_path(&relative_dir, &name);
                let metadata = fs::symlink_metadata(entry.path())
                    .map_err(|_| WorkspaceSnapshotError::MetadataRead)?;
                hash_frame(&mut hasher, b"path", relative.as_bytes());
                hash_frame(&mut hasher, b"metadata", &metadata_fingerprint(&metadata));
                if metadata.file_type().is_symlink() {
                    let target = fs::read_link(entry.path())
                        .map_err(|_| WorkspaceSnapshotError::FileRead)?;
                    hash_frame(&mut hasher, b"symlink", &path_bytes(target.as_os_str()));
                } else if metadata.is_dir() {
                    hash_frame(&mut hasher, b"kind", b"directory");
                    stack.push((entry.path(), relative));
                } else if metadata.is_file() {
                    self.record_hardlink(&metadata);
                    let byte_len = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
                    let within_file_limit = byte_len <= MAX_WORKSPACE_OPAQUE_FILE_HASH_BYTES;
                    let within_total_limit = budget
                        .opaque_hashed_content_bytes
                        .checked_add(byte_len)
                        .is_some_and(|total| total <= MAX_WORKSPACE_OPAQUE_TOTAL_HASH_BYTES);
                    if within_file_limit && within_total_limit {
                        let bytes =
                            fs::read(entry.path()).map_err(|_| WorkspaceSnapshotError::FileRead)?;
                        let stable_metadata = fs::symlink_metadata(entry.path())
                            .map_err(|_| WorkspaceSnapshotError::MetadataRead)?;
                        if bytes.len() != byte_len
                            || metadata_fingerprint(&metadata)
                                != metadata_fingerprint(&stable_metadata)
                        {
                            return Err(WorkspaceSnapshotError::FileRead);
                        }
                        budget.opaque_hashed_content_bytes += byte_len;
                        hash_frame(&mut hasher, b"content", &bytes);
                    } else {
                        hash_frame(&mut hasher, b"kind", b"metadata-only-regular");
                    }
                } else {
                    hash_frame(
                        &mut hasher,
                        b"kind",
                        special_file_kind(&metadata).as_str().as_bytes(),
                    );
                }
            }
        }
        Ok(format!("{:x}", hasher.finalize()))
    }
}

impl SnapshotEntry {
    fn metadata(
        kind: SnapshotKind,
        sha256: String,
        metadata: &fs::Metadata,
        byte_len: usize,
    ) -> Self {
        Self {
            kind,
            sha256,
            metadata_sha256: metadata_digest(metadata),
            byte_len,
            mode: metadata_mode(metadata),
            text: None,
            observed: false,
        }
    }

    fn regular(bytes: Vec<u8>, metadata: &fs::Metadata) -> Self {
        let retain_text = bytes.len() <= MAX_WORKSPACE_SNAPSHOT_FILE_BYTES;
        Self {
            kind: SnapshotKind::Regular,
            sha256: hash_bytes(&bytes),
            metadata_sha256: metadata_digest(metadata),
            byte_len: bytes.len(),
            mode: metadata_mode(metadata),
            text: retain_text.then(|| String::from_utf8(bytes).ok()).flatten(),
            observed: retain_text,
        }
    }
}

fn omitted(reason: &str) -> FileDiffProjection {
    FileDiffProjection {
        diff: None,
        truncated: false,
        truncation: "none",
        omitted_reason: Some(reason.to_owned()),
    }
}

fn unified_diff(source: &FileDiffSource<'_>) -> String {
    let mut options = DiffOptions::new();
    options.set_context_len(0);
    options.set_original_filename(if source.action == "add" {
        "/dev/null".to_owned()
    } else {
        format!("a/{}", source.path)
    });
    options.set_modified_filename(format!("b/{}", source.path));
    options
        .create_patch(source.before, source.after)
        .to_string()
}

fn bounded_diff(mut diff: String) -> (String, bool) {
    if diff.len() <= MAX_FILE_DIFF_BYTES {
        return (diff, false);
    }
    let mut end = MAX_FILE_DIFF_BYTES.saturating_sub(TRUNCATED_MARKER.len());
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    diff.truncate(end);
    diff.push_str(TRUNCATED_MARKER);
    (diff, true)
}

fn omission_reason(source: &FileDiffSource<'_>) -> Option<&'static str> {
    if source.before.contains('\0')
        || source.after.contains('\0')
        || unsupported_control(source.before)
        || unsupported_control(source.after)
    {
        Some("binary")
    } else if secret_like_path(source.path) || secret_like_text(source.before, source.after) {
        Some("secret-like")
    } else {
        None
    }
}

/// Whether a single file body is safe to retain as a workspace checkpoint
/// pre-image. Reuses the `file.diff` binary / secret-like policy so skipped
/// diffs and skipped checkpoints stay aligned.
pub fn content_is_checkpoint_safe(path: &str, content: &str) -> bool {
    !content.contains('\0')
        && !unsupported_control(content)
        && !secret_like_path(path)
        && !secret_like_text(content, "")
}

fn unsupported_control(text: &str) -> bool {
    text.chars()
        .any(|ch| ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t')
}

fn secret_like_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path == ".env"
        || path.contains("/.env")
        || path.contains("/.ssh/")
        || path.ends_with(".pem")
        || path.ends_with(".key")
}

/// Heuristic, not a guarantee: substring needles over the lowercased text.
/// Bare key names (not `key=` forms) so JSON/YAML/env spellings all hit;
/// over-matching only withholds diff content, which is the safe direction.
fn secret_like_text(before: &str, after: &str) -> bool {
    let text = format!("{before}\n{after}").to_ascii_lowercase();
    [
        "-----begin ",
        "authorization:",
        "api_key",
        "apikey",
        "access_token",
        "access_key",
        "refresh_token",
        "password",
        "passwd",
        "secret",
        "token=",
        "\"token\"",
        "private_key",
        "client_id=",
        "credential",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn observed_diff(
    path: &str,
    action: &'static str,
    before: &str,
    after: Option<&str>,
) -> FileDiffProjection {
    let Some(after) = after else {
        return omitted("binary");
    };
    file_diff_projection(FileDiffSource {
        path,
        action,
        before,
        after,
    })
}

fn observed_change(
    workspace_root: String,
    path: String,
    before: Option<&SnapshotEntry>,
    after: Option<&SnapshotEntry>,
) -> Option<ObservedFileChange> {
    match (before, after) {
        (None, Some(after)) => Some(added_change(workspace_root, path, after)),
        (Some(before), None) => Some(deleted_change(workspace_root, path, before)),
        (Some(before), Some(after)) if file_changed(before, after) => {
            Some(modified_change(workspace_root, path, before, after))
        }
        _ => None,
    }
}

fn added_change(workspace_root: String, path: String, after: &SnapshotEntry) -> ObservedFileChange {
    ObservedFileChange {
        workspace_root,
        path,
        file_type: after.kind.as_str(),
        before_file_type: None,
        after_file_type: Some(after.kind.as_str()),
        action: "add",
        before_sha256: None,
        after_sha256: Some(after.sha256.clone()),
        before_metadata_sha256: None,
        after_metadata_sha256: Some(after.metadata_sha256.clone()),
        before_byte_len: 0,
        after_byte_len: after.byte_len,
        before_mode: None,
        after_mode: Some(after.mode),
        before_text: None,
        after_text: after.text.clone(),
        diff_omitted_reason: content_omitted_reason(after),
    }
}

fn modified_change(
    workspace_root: String,
    path: String,
    before: &SnapshotEntry,
    after: &SnapshotEntry,
) -> ObservedFileChange {
    ObservedFileChange {
        workspace_root,
        path,
        file_type: after.kind.as_str(),
        before_file_type: Some(before.kind.as_str()),
        after_file_type: Some(after.kind.as_str()),
        action: "modify",
        before_sha256: Some(before.sha256.clone()),
        after_sha256: Some(after.sha256.clone()),
        before_metadata_sha256: Some(before.metadata_sha256.clone()),
        after_metadata_sha256: Some(after.metadata_sha256.clone()),
        before_byte_len: before.byte_len,
        after_byte_len: after.byte_len,
        before_mode: Some(before.mode),
        after_mode: Some(after.mode),
        before_text: before.text.clone(),
        after_text: after.text.clone(),
        diff_omitted_reason: paired_content_omitted_reason(before, after),
    }
}

fn deleted_change(
    workspace_root: String,
    path: String,
    before: &SnapshotEntry,
) -> ObservedFileChange {
    ObservedFileChange {
        workspace_root,
        path,
        file_type: before.kind.as_str(),
        before_file_type: Some(before.kind.as_str()),
        after_file_type: None,
        action: "delete",
        before_sha256: Some(before.sha256.clone()),
        after_sha256: None,
        before_metadata_sha256: Some(before.metadata_sha256.clone()),
        after_metadata_sha256: None,
        before_byte_len: before.byte_len,
        after_byte_len: 0,
        before_mode: Some(before.mode),
        after_mode: None,
        before_text: None,
        after_text: None,
        diff_omitted_reason: Some(deleted_content_omitted_reason(before)),
    }
}

fn file_changed(before: &SnapshotEntry, after: &SnapshotEntry) -> bool {
    before.kind != after.kind
        || before.sha256 != after.sha256
        || before.metadata_sha256 != after.metadata_sha256
}

fn paired_content_omitted_reason(
    before: &SnapshotEntry,
    after: &SnapshotEntry,
) -> Option<&'static str> {
    if before.kind != after.kind {
        return Some("file-type-change");
    }
    if before.sha256 == after.sha256 && before.metadata_sha256 != after.metadata_sha256 {
        return Some("metadata-only");
    }
    content_omitted_reason(before).or_else(|| content_omitted_reason(after))
}

fn content_omitted_reason(file: &SnapshotEntry) -> Option<&'static str> {
    match file.kind {
        SnapshotKind::Regular if file.observed => None,
        SnapshotKind::Regular => Some("content-unobserved"),
        SnapshotKind::Symlink => Some("symlink-target"),
        SnapshotKind::Directory => Some("directory-metadata"),
        SnapshotKind::OpaqueDirectory => Some("excluded-surface"),
        SnapshotKind::ReadOnlyDirectory => Some("read-only-surface"),
        SnapshotKind::Fifo
        | SnapshotKind::Socket
        | SnapshotKind::BlockDevice
        | SnapshotKind::CharDevice
        | SnapshotKind::Other => Some("special-file"),
    }
}

fn deleted_content_omitted_reason(file: &SnapshotEntry) -> &'static str {
    match file.kind {
        SnapshotKind::Regular => "delete-content",
        _ => content_omitted_reason(file).unwrap_or("delete-content"),
    }
}

fn discover_read_only_surfaces(
    root: &Path,
) -> Result<BTreeMap<String, FrozenReadOnlySurface>, WorkspaceSnapshotError> {
    let requested_root = root.to_path_buf();
    let root = requested_root
        .canonicalize()
        .map_err(|_| WorkspaceSnapshotError::InvalidRoot)?;
    if root != requested_root {
        return Err(WorkspaceSnapshotError::InvalidRoot);
    }
    crate::sandbox::freeze_read_only_workspace_surfaces(&root)
        .map_err(|_| WorkspaceSnapshotError::ProtectedSurfaceIdentity)?
        .into_iter()
        .map(|surface| {
            let relative = surface
                .path()
                .strip_prefix(&root)
                .ok()
                .and_then(Path::to_str)
                .map(str::to_owned)
                .ok_or(WorkspaceSnapshotError::InvalidRoot)?;
            Ok((relative, surface))
        })
        .collect()
}

fn validate_read_only_surfaces(
    root: &Path,
    surfaces: &BTreeMap<String, FrozenReadOnlySurface>,
) -> Result<(), WorkspaceSnapshotError> {
    let surfaces = surfaces.values().cloned().collect::<Vec<_>>();
    crate::sandbox::validate_frozen_read_only_surfaces(
        std::slice::from_ref(&root.to_path_buf()),
        &surfaces,
    )
    .map_err(|_| WorkspaceSnapshotError::ProtectedSurfaceIdentity)
}

fn read_sorted_dir_bounded(
    dir: &Path,
    max_entries: usize,
    limit_error: WorkspaceSnapshotError,
) -> Result<Vec<DirEntry>, WorkspaceSnapshotError> {
    let read_dir = fs::read_dir(dir).map_err(|_| WorkspaceSnapshotError::DirectoryRead)?;
    let mut entries = Vec::with_capacity(max_entries.min(256));
    for entry in read_dir {
        if entries.len() >= max_entries {
            return Err(limit_error);
        }
        entries.push(entry.map_err(|_| WorkspaceSnapshotError::DirectoryRead)?);
    }
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

fn relative_path(relative_dir: &str, name: &str) -> String {
    if relative_dir.is_empty() {
        name.to_owned()
    } else {
        format!("{relative_dir}/{name}")
    }
}

fn ignored_dir(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(
            ".git"
                | ".euler"
                | ".mypy_cache"
                | ".next"
                | ".pytest_cache"
                | "__pycache__"
                | "build"
                | "dist"
                | "node_modules"
                | "target"
                | "vendor"
        )
    )
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hash_frame(hasher: &mut Sha256, tag: &[u8], bytes: &[u8]) {
    hasher.update(u64::try_from(tag.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(tag);
    hasher.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(unix)]
fn path_bytes(path: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;

    path.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &OsStr) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

fn metadata_digest(metadata: &fs::Metadata) -> String {
    hash_bytes(&metadata_fingerprint(metadata))
}

#[cfg(unix)]
fn metadata_fingerprint(metadata: &fs::Metadata) -> Vec<u8> {
    use std::os::unix::fs::MetadataExt;

    let mut bytes = Vec::with_capacity(80);
    bytes.extend_from_slice(&metadata.dev().to_le_bytes());
    bytes.extend_from_slice(&metadata.ino().to_le_bytes());
    bytes.extend_from_slice(&metadata.mode().to_le_bytes());
    bytes.extend_from_slice(&metadata.nlink().to_le_bytes());
    bytes.extend_from_slice(&metadata.uid().to_le_bytes());
    bytes.extend_from_slice(&metadata.gid().to_le_bytes());
    bytes.extend_from_slice(&metadata.size().to_le_bytes());
    bytes.extend_from_slice(&metadata.mtime().to_le_bytes());
    bytes.extend_from_slice(&metadata.mtime_nsec().to_le_bytes());
    bytes.extend_from_slice(&metadata.ctime().to_le_bytes());
    bytes.extend_from_slice(&metadata.ctime_nsec().to_le_bytes());
    bytes
}

#[cfg(not(unix))]
fn metadata_fingerprint(metadata: &fs::Metadata) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&metadata.len().to_le_bytes());
    bytes.push(u8::from(metadata.permissions().readonly()));
    if let Ok(modified) = metadata.modified() {
        if let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH) {
            bytes.extend_from_slice(&duration.as_secs().to_le_bytes());
            bytes.extend_from_slice(&duration.subsec_nanos().to_le_bytes());
        }
    }
    bytes
}

#[cfg(unix)]
fn metadata_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;

    metadata.mode() & 0o7777
}

#[cfg(not(unix))]
fn metadata_mode(metadata: &fs::Metadata) -> u32 {
    if metadata.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}

#[cfg(unix)]
fn hardlink_identity(metadata: &fs::Metadata) -> Option<(u64, u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    Some((metadata.dev(), metadata.ino(), metadata.nlink()))
}

#[cfg(not(unix))]
fn hardlink_identity(_metadata: &fs::Metadata) -> Option<(u64, u64, u64)> {
    None
}

#[cfg(unix)]
fn special_file_kind(metadata: &fs::Metadata) -> SnapshotKind {
    use std::os::unix::fs::FileTypeExt;

    let kind = metadata.file_type();
    if kind.is_fifo() {
        SnapshotKind::Fifo
    } else if kind.is_socket() {
        SnapshotKind::Socket
    } else if kind.is_block_device() {
        SnapshotKind::BlockDevice
    } else if kind.is_char_device() {
        SnapshotKind::CharDevice
    } else {
        SnapshotKind::Other
    }
}

#[cfg(not(unix))]
fn special_file_kind(_metadata: &fs::Metadata) -> SnapshotKind {
    SnapshotKind::Other
}

fn optional_string(value: &Option<String>) -> Value {
    value
        .as_ref()
        .map_or(Value::Null, |value| value.clone().into())
}

fn optional_static_str(value: Option<&'static str>) -> Value {
    value.map_or(Value::Null, Into::into)
}

fn optional_u32(value: Option<u32>) -> Value {
    value.map_or(Value::Null, Into::into)
}

#[cfg(test)]
mod secret_detector_tests {
    use super::{
        read_sorted_dir_bounded, secret_like_text, CaptureBudget, WorkspaceSnapshotError,
        MAX_WORKSPACE_OPAQUE_ENTRIES,
    };

    #[test]
    fn detector_catches_structured_and_env_spellings() {
        // Review finding: `password=`-style needles missed JSON keys and
        // AWS_SECRET_ACCESS_KEY entirely.
        for text in [
            "{\"password\": \"hunter2\"}",
            "password: hunter2",
            "AWS_SECRET_ACCESS_KEY=abc123",
            "aws_access_key_id = AKIA...",
            "{\"client_secret\": \"x\"}",
            "PRIVATE_KEY=-----",
            "apiKey: xyz",
            "credentials.json contents",
        ] {
            assert!(secret_like_text(text, ""), "detector must flag: {text}");
        }
        assert!(!secret_like_text("fn max_tokens(&self) -> u64 { 42 }", ""));
        assert!(!secret_like_text("let x = compute_totals();", ""));
    }

    #[test]
    fn bounded_directory_read_stops_at_the_requested_limit() {
        let temp = tempfile::tempdir().expect("temp dir");
        std::fs::write(temp.path().join("a"), "a").expect("first file");
        std::fs::write(temp.path().join("b"), "b").expect("second file");

        assert_eq!(
            read_sorted_dir_bounded(temp.path(), 1, WorkspaceSnapshotError::FileLimit)
                .expect_err("second entry exceeds limit"),
            WorkspaceSnapshotError::FileLimit
        );
    }

    #[test]
    fn opaque_entry_budget_is_global_across_surfaces() {
        let mut budget = CaptureBudget::default();
        budget
            .record_opaque_entries(MAX_WORKSPACE_OPAQUE_ENTRIES - 1)
            .expect("first surface");
        budget
            .record_opaque_entries(1)
            .expect("last globally available entry");
        assert_eq!(
            budget.record_opaque_entries(1),
            Err(WorkspaceSnapshotError::OpaqueEntryLimit)
        );
    }
}
