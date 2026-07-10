use diffy::DiffOptions;
use euler_event::{object, JsonObject};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, DirEntry};
use std::io;
use std::path::{Path, PathBuf};

pub const MAX_FILE_DIFF_BYTES: usize = 64 * 1024;
pub const MAX_WORKSPACE_SNAPSHOT_FILES: usize = 4_096;
pub const MAX_WORKSPACE_SNAPSHOT_FILE_BYTES: usize = 256 * 1024;
pub const MAX_WORKSPACE_SNAPSHOT_TOTAL_BYTES: usize = 64 * 1024 * 1024;

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
    pub path: String,
    pub action: &'static str,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
    pub before_byte_len: usize,
    pub after_byte_len: usize,
    before_text: Option<String>,
    after_text: Option<String>,
    diff_omitted_reason: Option<&'static str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSnapshot {
    complete: bool,
    files: BTreeMap<String, SnapshotFile>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SnapshotFile {
    sha256: Option<String>,
    byte_len: usize,
    text: Option<String>,
    observed: bool,
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
    origin: &'static str,
    change: &ObservedFileChange,
) -> JsonObject {
    object([
        ("tool_call_id", tool_call_id.to_owned().into()),
        ("origin", origin.into()),
        ("action", change.action.into()),
        ("path", change.path.clone().into()),
        ("old_path", Value::Null),
        ("before_sha256", optional_string(&change.before_sha256)),
        ("after_sha256", optional_string(&change.after_sha256)),
        ("before_byte_len", change.before_byte_len.into()),
        ("after_byte_len", change.after_byte_len.into()),
        ("diff_redaction", "omitted".into()),
    ])
}

pub fn observed_file_diff_payload(
    tool_call_id: &str,
    file_change_id: &str,
    origin: &'static str,
    change: &ObservedFileChange,
) -> JsonObject {
    let projection = observed_file_diff_projection(change);
    object([
        ("tool_call_id", tool_call_id.to_owned().into()),
        ("file_change_id", file_change_id.to_owned().into()),
        ("path", change.path.clone().into()),
        ("old_path", Value::Null),
        ("action", change.action.into()),
        ("origin", origin.into()),
        ("before_sha256", optional_string(&change.before_sha256)),
        ("after_sha256", optional_string(&change.after_sha256)),
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

pub fn capture_workspace_snapshot(root: &Path) -> io::Result<WorkspaceSnapshot> {
    WorkspaceSnapshot::capture(root)
}

impl WorkspaceSnapshot {
    pub fn changes_to(&self, after: &Self) -> Vec<ObservedFileChange> {
        if !self.complete || !after.complete {
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
                observed_change(path, before_file, after_file)
            })
            .collect()
    }

    fn capture(root: &Path) -> io::Result<Self> {
        let root = root.canonicalize()?;
        let mut snapshot = Self {
            complete: true,
            files: BTreeMap::new(),
        };
        let mut stack = vec![(root, String::new())];
        let mut total_bytes = 0usize;
        while let Some((dir, relative_dir)) = stack.pop() {
            let Some(entries) = read_sorted_dir(&dir) else {
                return Ok(Self::incomplete());
            };
            if !snapshot.record_entries(entries, &relative_dir, &mut stack, &mut total_bytes) {
                return Ok(Self::incomplete());
            }
        }
        Ok(snapshot)
    }

    fn incomplete() -> Self {
        Self {
            complete: false,
            files: BTreeMap::new(),
        }
    }

    fn record_entries(
        &mut self,
        entries: Vec<DirEntry>,
        relative_dir: &str,
        stack: &mut Vec<(PathBuf, String)>,
        total_bytes: &mut usize,
    ) -> bool {
        for entry in entries {
            if !self.record_entry(entry, relative_dir, stack, total_bytes) {
                return false;
            }
        }
        true
    }

    fn record_entry(
        &mut self,
        entry: DirEntry,
        relative_dir: &str,
        stack: &mut Vec<(PathBuf, String)>,
        total_bytes: &mut usize,
    ) -> bool {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return true;
        };
        let path = relative_path(relative_dir, &name);
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            return false;
        };
        if metadata.file_type().is_symlink() {
            return true;
        }
        if metadata.is_dir() {
            if !ignored_dir(OsStr::new(&name)) {
                stack.push((entry.path(), path));
            }
            return true;
        }
        if metadata.is_file() {
            return self.record_file(entry.path(), path, metadata.len(), total_bytes);
        }
        true
    }

    fn record_file(
        &mut self,
        path: PathBuf,
        relative: String,
        byte_len: u64,
        total_bytes: &mut usize,
    ) -> bool {
        if self.files.len() >= MAX_WORKSPACE_SNAPSHOT_FILES {
            return false;
        }
        let Ok(byte_len) = usize::try_from(byte_len) else {
            return false;
        };
        if byte_len > MAX_WORKSPACE_SNAPSHOT_FILE_BYTES {
            self.files
                .insert(relative, SnapshotFile::unobserved(byte_len));
            return true;
        }
        let Some(next_total) = total_bytes.checked_add(byte_len) else {
            return false;
        };
        if next_total > MAX_WORKSPACE_SNAPSHOT_TOTAL_BYTES {
            return false;
        }
        let Ok(bytes) = fs::read(path) else {
            return false;
        };
        *total_bytes = next_total;
        self.files.insert(relative, SnapshotFile::observed(bytes));
        true
    }
}

impl SnapshotFile {
    fn observed(bytes: Vec<u8>) -> Self {
        Self {
            sha256: Some(hash_bytes(&bytes)),
            byte_len: bytes.len(),
            text: String::from_utf8(bytes).ok(),
            observed: true,
        }
    }

    fn unobserved(byte_len: usize) -> Self {
        Self {
            sha256: None,
            byte_len,
            text: None,
            observed: false,
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

fn secret_like_text(before: &str, after: &str) -> bool {
    let text = format!("{before}\n{after}").to_ascii_lowercase();
    [
        "-----begin ",
        "authorization: bearer ",
        "api_key",
        "access_token",
        "refresh_token",
        "password=",
        "secret=",
        "token=",
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
    path: String,
    before: Option<&SnapshotFile>,
    after: Option<&SnapshotFile>,
) -> Option<ObservedFileChange> {
    match (before, after) {
        (None, Some(after)) => Some(added_change(path, after)),
        (Some(before), None) => Some(deleted_change(path, before)),
        (Some(before), Some(after)) if file_changed(before, after) => {
            Some(modified_change(path, before, after))
        }
        _ => None,
    }
}

fn added_change(path: String, after: &SnapshotFile) -> ObservedFileChange {
    ObservedFileChange {
        path,
        action: "add",
        before_sha256: None,
        after_sha256: after.sha256.clone(),
        before_byte_len: 0,
        after_byte_len: after.byte_len,
        before_text: None,
        after_text: after.text.clone(),
        diff_omitted_reason: content_omitted_reason(after),
    }
}

fn modified_change(
    path: String,
    before: &SnapshotFile,
    after: &SnapshotFile,
) -> ObservedFileChange {
    ObservedFileChange {
        path,
        action: "modify",
        before_sha256: before.sha256.clone(),
        after_sha256: after.sha256.clone(),
        before_byte_len: before.byte_len,
        after_byte_len: after.byte_len,
        before_text: before.text.clone(),
        after_text: after.text.clone(),
        diff_omitted_reason: paired_content_omitted_reason(before, after),
    }
}

fn deleted_change(path: String, before: &SnapshotFile) -> ObservedFileChange {
    ObservedFileChange {
        path,
        action: "delete",
        before_sha256: before.sha256.clone(),
        after_sha256: None,
        before_byte_len: before.byte_len,
        after_byte_len: 0,
        before_text: None,
        after_text: None,
        diff_omitted_reason: Some("delete-content"),
    }
}

fn file_changed(before: &SnapshotFile, after: &SnapshotFile) -> bool {
    match (&before.sha256, &after.sha256) {
        (Some(before), Some(after)) => before != after,
        _ => before.byte_len != after.byte_len,
    }
}

fn paired_content_omitted_reason(
    before: &SnapshotFile,
    after: &SnapshotFile,
) -> Option<&'static str> {
    if !before.observed || !after.observed {
        Some("content-unobserved")
    } else {
        content_omitted_reason(before).or_else(|| content_omitted_reason(after))
    }
}

fn content_omitted_reason(file: &SnapshotFile) -> Option<&'static str> {
    if !file.observed {
        Some("content-unobserved")
    } else {
        None
    }
}

fn read_sorted_dir(dir: &Path) -> Option<Vec<DirEntry>> {
    let mut entries = fs::read_dir(dir)
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    entries.sort_by_key(|entry| entry.file_name());
    Some(entries)
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

fn optional_string(value: &Option<String>) -> Value {
    value
        .as_ref()
        .map_or(Value::Null, |value| value.clone().into())
}
