//! Scoped permission grants: session and project stores.
//!
//! Capability approval modes (`ask` / `session-allow` / `always-deny`) remain the
//! coarse gate. Scoped grants sit above `ask`: a matching session or project
//! grant allows a request without re-prompting. Project grants persist under
//! the workspace `.euler/` directory; every project-grant write is a user
//! decision that callers must record in provenance (see capabilities contract).

use euler_sdk::Capability;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Max bytes for a scope pattern (command token or directory prefix).
pub const MAX_SCOPE_PATTERN_BYTES: usize = 256;
/// Max bytes retained on [`crate::permissions::PermissionRequest::command`].
pub const MAX_GRANT_COMMAND_BYTES: usize = 4 * 1024;
/// Max bytes retained on deny-with-instruction text.
pub const MAX_GRANT_INSTRUCTION_BYTES: usize = 4 * 1024;
/// Max bytes for the project grants file.
const MAX_GRANTS_FILE_BYTES: u64 = 64 * 1024;

const GRANTS_FILE: &str = "grants.json";
const EULER_DIR: &str = ".euler";
const GRANTS_VERSION: u64 = 1;

/// Opaque bounded pattern for a scoped grant.
///
/// Empty means **unscoped**: the whole capability (legacy `AllowSession`).
/// For `shell-exec`, non-empty is the command first token (`cargo`, `git`).
/// For `fs-write`, non-empty is a workspace-relative directory prefix (typically
/// the path's top-level component). Derivation of the pattern from a live
/// request is a caller concern; this type only stores and matches.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ScopePattern(String);

impl ScopePattern {
    /// Unscoped pattern: matches any request for the capability.
    pub fn unscoped() -> Self {
        Self(String::new())
    }

    /// Build a pattern, rejecting oversize or control-bearing strings.
    pub fn new(raw: impl Into<String>) -> Result<Self, ScopePatternError> {
        let raw = raw.into();
        validate_pattern(&raw)?;
        Ok(Self(raw))
    }

    pub fn is_unscoped(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ScopePattern {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum ScopePatternError {
    #[error("scope pattern exceeds {MAX_SCOPE_PATTERN_BYTES} bytes")]
    TooLarge,
    #[error("scope pattern contains control characters")]
    ControlChars,
}

fn validate_pattern(raw: &str) -> Result<(), ScopePatternError> {
    if raw.len() > MAX_SCOPE_PATTERN_BYTES {
        return Err(ScopePatternError::TooLarge);
    }
    if raw.chars().any(|c| c.is_control()) {
        return Err(ScopePatternError::ControlChars);
    }
    Ok(())
}

/// How long a grant lasts and which pattern it covers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GrantScope {
    /// Allow this request only; do not retain a grant.
    Once,
    /// Retain for the current session under `ScopePattern`.
    Session(ScopePattern),
    /// Persist under the project grants file under `ScopePattern`.
    Project(ScopePattern),
}

impl GrantScope {
    pub fn pattern(&self) -> Option<&ScopePattern> {
        match self {
            Self::Once => None,
            Self::Session(p) | Self::Project(p) => Some(p),
        }
    }

    /// Wire label for permission.decision payloads: `once` | `session` | `project`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Session(_) => "session",
            Self::Project(_) => "project",
        }
    }
}

/// One active grant entry (session memory or project file).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveGrant {
    pub capability: Capability,
    pub pattern: ScopePattern,
}

impl ActiveGrant {
    pub fn new(capability: Capability, pattern: ScopePattern) -> Self {
        Self {
            capability,
            pattern,
        }
    }

    pub fn unscoped(capability: Capability) -> Self {
        Self::new(capability, ScopePattern::unscoped())
    }

    /// Whether this grant covers `capability` under the request context.
    pub fn matches(
        &self,
        capability: Capability,
        command: Option<&str>,
        path: Option<&Path>,
    ) -> bool {
        if self.capability != capability {
            return false;
        }
        if self.pattern.is_unscoped() {
            return true;
        }
        match capability {
            Capability::ShellExec => command
                .and_then(command_first_token)
                .is_some_and(|token| token == self.pattern.as_str()),
            Capability::FsWrite => {
                path.is_some_and(|p| path_under_prefix(p, self.pattern.as_str()))
            }
            // Patterned grants for other capabilities are exact unscoped-only
            // until those capabilities gain structured scope fields.
            _ => false,
        }
    }
}

/// First whitespace-delimited token of a shell command line.
pub fn command_first_token(command: &str) -> Option<&str> {
    let token = command.split_whitespace().next()?;
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Workspace-relative path is the prefix itself or a descendant.
pub fn path_under_prefix(path: &Path, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    let path = path_as_relative_str(path);
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

fn path_as_relative_str(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches("./")
        .replace('\\', "/")
}

/// Bound a command string for request enrichment.
pub fn bound_command(command: &str) -> Option<String> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(bound_utf8(trimmed, MAX_GRANT_COMMAND_BYTES))
}

/// Bound deny-instruction text.
pub fn bound_instruction(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(bound_utf8(trimmed, MAX_GRANT_INSTRUCTION_BYTES))
}

fn bound_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// In-memory grant list with membership and revoke helpers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GrantList {
    grants: Vec<ActiveGrant>,
}

impl GrantList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_vec(grants: Vec<ActiveGrant>) -> Self {
        Self { grants }
    }

    pub fn as_slice(&self) -> &[ActiveGrant] {
        &self.grants
    }

    pub fn iter(&self) -> impl Iterator<Item = &ActiveGrant> {
        self.grants.iter()
    }

    pub fn is_granted(
        &self,
        capability: Capability,
        command: Option<&str>,
        path: Option<&Path>,
    ) -> bool {
        self.grants
            .iter()
            .any(|g| g.matches(capability, command, path))
    }

    pub fn insert(&mut self, grant: ActiveGrant) {
        if !self.grants.iter().any(|g| g == &grant) {
            self.grants.push(grant);
        }
    }

    /// Remove grants matching capability + pattern. Returns how many were removed.
    pub fn revoke(&mut self, capability: Capability, pattern: &ScopePattern) -> usize {
        let before = self.grants.len();
        self.grants
            .retain(|g| !(g.capability == capability && g.pattern == *pattern));
        before - self.grants.len()
    }

    pub fn clear(&mut self) {
        self.grants.clear();
    }
}

/// Project-local grants file under `<root>/.euler/grants.json`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectGrantStore {
    path: PathBuf,
}

impl ProjectGrantStore {
    pub fn for_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            path: root.join(EULER_DIR).join(GRANTS_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<GrantList, ProjectGrantError> {
        let content = match fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(GrantList::new());
            }
            Err(source) => {
                return Err(ProjectGrantError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        if content.len() as u64 > MAX_GRANTS_FILE_BYTES {
            return Err(ProjectGrantError::TooLarge {
                path: self.path.clone(),
                bytes: content.len() as u64,
            });
        }
        let doc: GrantsFile =
            serde_json::from_str(&content).map_err(|source| ProjectGrantError::Invalid {
                path: self.path.clone(),
                source,
            })?;
        if doc.version != GRANTS_VERSION {
            return Err(ProjectGrantError::UnsupportedVersion {
                path: self.path.clone(),
                version: doc.version,
            });
        }
        let mut list = GrantList::new();
        for entry in doc.grants {
            let capability = Capability::parse(&entry.capability).ok_or_else(|| {
                ProjectGrantError::UnknownCapability {
                    path: self.path.clone(),
                    capability: entry.capability,
                }
            })?;
            let pattern = ScopePattern::new(entry.pattern).map_err(|source| {
                ProjectGrantError::BadPattern {
                    path: self.path.clone(),
                    source,
                }
            })?;
            list.insert(ActiveGrant::new(capability, pattern));
        }
        Ok(list)
    }

    pub fn save(&self, grants: &GrantList) -> Result<(), ProjectGrantError> {
        let dir = self.path.parent().ok_or_else(|| ProjectGrantError::Io {
            path: self.path.clone(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "grants path has no parent"),
        })?;
        fs::create_dir_all(dir).map_err(|source| ProjectGrantError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }
        let doc = GrantsFile {
            version: GRANTS_VERSION,
            grants: grants
                .iter()
                .map(|g| GrantFileEntry {
                    capability: g.capability.as_str().to_owned(),
                    pattern: g.pattern.as_str().to_owned(),
                })
                .collect(),
        };
        let bytes =
            serde_json::to_vec_pretty(&doc).map_err(|source| ProjectGrantError::Serialize {
                path: self.path.clone(),
                source,
            })?;
        write_atomic(&self.path, &bytes)
    }

    pub fn add(&self, grant: &ActiveGrant) -> Result<GrantList, ProjectGrantError> {
        let mut list = self.load()?;
        list.insert(grant.clone());
        self.save(&list)?;
        Ok(list)
    }

    pub fn revoke(
        &self,
        capability: Capability,
        pattern: &ScopePattern,
    ) -> Result<GrantList, ProjectGrantError> {
        let mut list = self.load()?;
        list.revoke(capability, pattern);
        self.save(&list)?;
        Ok(list)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct GrantsFile {
    version: u64,
    #[serde(default)]
    grants: Vec<GrantFileEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GrantFileEntry {
    capability: String,
    #[serde(default)]
    pattern: String,
}

#[derive(Debug, Error)]
pub enum ProjectGrantError {
    #[error("project grant store is not loaded; call load_project_grants first")]
    NoStore,
    #[error("project grants file too large at {}: {bytes} bytes", path.display())]
    TooLarge { path: PathBuf, bytes: u64 },
    #[error("unsupported project grants version {version} at {}", path.display())]
    UnsupportedVersion { path: PathBuf, version: u64 },
    #[error("unknown capability `{capability}` in {}", path.display())]
    UnknownCapability { path: PathBuf, capability: String },
    #[error("invalid scope pattern in {}: {source}", path.display())]
    BadPattern {
        path: PathBuf,
        #[source]
        source: ScopePatternError,
    },
    #[error("invalid project grants file {}: {source}", path.display())]
    Invalid {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize project grants at {}: {source}", path.display())]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("project grants I/O at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ProjectGrantError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let temp_path = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("grants.json"),
        ulid::Ulid::new()
    ));
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp_path)
            .map_err(|source| ProjectGrantError::Io {
                path: temp_path.clone(),
                source,
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
        }
        file.write_all(bytes)
            .map_err(|source| ProjectGrantError::Io {
                path: temp_path.clone(),
                source,
            })?;
        file.flush().map_err(|source| ProjectGrantError::Io {
            path: temp_path.clone(),
            source,
        })?;
        file.sync_data().map_err(|source| ProjectGrantError::Io {
            path: temp_path.clone(),
            source,
        })?;
    }
    fs::rename(&temp_path, path).map_err(|source| ProjectGrantError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // Best-effort directory sync on Unix.
    #[cfg(unix)]
    {
        let _ = File::open(dir).and_then(|f| f.sync_all());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unscoped_matches_any_context() {
        let grant = ActiveGrant::unscoped(Capability::ShellExec);
        assert!(grant.matches(Capability::ShellExec, Some("cargo test"), None));
        assert!(grant.matches(Capability::ShellExec, None, None));
        assert!(!grant.matches(Capability::FsWrite, None, None));
    }

    #[test]
    fn shell_pattern_matches_first_token() {
        let grant = ActiveGrant::new(
            Capability::ShellExec,
            ScopePattern::new("cargo").expect("pattern"),
        );
        assert!(grant.matches(Capability::ShellExec, Some("cargo test -q"), None));
        assert!(grant.matches(Capability::ShellExec, Some("  cargo"), None));
        assert!(!grant.matches(Capability::ShellExec, Some("git status"), None));
        assert!(!grant.matches(Capability::ShellExec, None, None));
    }

    #[test]
    fn fs_write_pattern_matches_directory_prefix() {
        let grant = ActiveGrant::new(
            Capability::FsWrite,
            ScopePattern::new("src").expect("pattern"),
        );
        assert!(grant.matches(Capability::FsWrite, None, Some(Path::new("src/main.rs"))));
        assert!(grant.matches(Capability::FsWrite, None, Some(Path::new("src"))));
        assert!(!grant.matches(Capability::FsWrite, None, Some(Path::new("crates/foo.rs"))));
    }

    #[test]
    fn project_store_round_trips() {
        let temp = tempfile::tempdir().expect("temp");
        let store = ProjectGrantStore::for_root(temp.path());
        let grant = ActiveGrant::new(
            Capability::ShellExec,
            ScopePattern::new("git").expect("pattern"),
        );
        store.add(&grant).expect("add");
        let loaded = store.load().expect("load");
        assert_eq!(loaded.as_slice(), std::slice::from_ref(&grant));
        store
            .revoke(Capability::ShellExec, &ScopePattern::new("git").expect("p"))
            .expect("revoke");
        assert!(store.load().expect("load").as_slice().is_empty());
    }

    #[test]
    fn scope_pattern_rejects_control_and_oversize() {
        assert!(ScopePattern::new("a\nb").is_err());
        assert!(ScopePattern::new("x".repeat(MAX_SCOPE_PATTERN_BYTES + 1)).is_err());
        assert!(ScopePattern::new("x".repeat(MAX_SCOPE_PATTERN_BYTES)).is_ok());
    }

    #[test]
    fn grant_list_dedupes_and_revokes() {
        let mut list = GrantList::new();
        let g = ActiveGrant::new(
            Capability::FsWrite,
            ScopePattern::new("docs").expect("pattern"),
        );
        list.insert(g.clone());
        list.insert(g.clone());
        assert_eq!(list.as_slice().len(), 1);
        assert_eq!(list.revoke(Capability::FsWrite, &g.pattern), 1);
        assert!(list.as_slice().is_empty());
    }
}
