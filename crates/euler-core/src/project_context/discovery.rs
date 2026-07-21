//! Secure `EULER.md` discovery along the project-root-to-workspace chain.
//!
//! Containment rules (ADR 0017 / project-context contract):
//! - the project discovery root is the nearest ancestor with an exact `.git`
//!   entry that is a regular file or directory; a symlinked `.git` is not a
//!   marker, and a `.git` file (linked worktree / submodule) is a root whose
//!   contents are never followed;
//! - every open is relative to a held directory handle with no-follow
//!   semantics; symlinks, reparse points, devices, FIFOs, and other
//!   non-regular files are rejected after the handle is verified;
//! - directory entries are compared by exact name — a case-insensitive
//!   filesystem lookup must never admit `euler.md` as `EULER.md`;
//! - reads follow the stable-read protocol: one handle per attempt, stable
//!   metadata compared before and after the bounded read, one retry, then a
//!   typed `changed_during_read` omission;
//! - malformed, unsafe, or over-limit sources are omitted whole with typed
//!   content-free diagnostics, never truncated;
//! - discovery reads the working tree and ignores version-control state.
//!
//! A platform without no-follow reads omits every source with a
//! `no_follow_unsupported` diagnostic instead of following symlinks.

use super::digest::source_digest_v1;
use super::manifest::{ManifestDiagnostic, ManifestSource};
use super::{
    MAX_CHAIN_LEVELS, MAX_COMBINED_EULER_MD_BYTES, MAX_EULER_MD_BYTES, MAX_EULER_MD_SOURCES,
    MAX_IDENTITY_BYTES,
};
use crate::redaction::SecretRedactor;
use std::path::Path;

pub(crate) const EULER_MD_FILE_NAME: &str = "EULER.md";

/// Stable, content-free diagnostic reason codes. These are recorded in
/// provenance and inside the candidate manifest; changing one changes
/// candidate digests, so treat the set as append-only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiagnosticReason {
    NotRegularFile,
    SymlinkRejected,
    ChangedDuringRead,
    SourceTooLarge,
    CombinedLimitExceeded,
    SourceCountExceeded,
    InvalidUtf8,
    CaseMismatch,
    NonUtf8Path,
    IdentityTooLong,
    ChainDepthExceeded,
    IoError,
    /// Constructed only on platforms without a ratified no-follow read path.
    #[cfg_attr(unix, allow(dead_code))]
    NoFollowUnsupported,
}

impl DiagnosticReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NotRegularFile => "not_regular_file",
            Self::SymlinkRejected => "symlink_rejected",
            Self::ChangedDuringRead => "changed_during_read",
            Self::SourceTooLarge => "source_too_large",
            Self::CombinedLimitExceeded => "combined_limit_exceeded",
            Self::SourceCountExceeded => "source_count_exceeded",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::CaseMismatch => "case_mismatch",
            Self::NonUtf8Path => "non_utf8_path",
            Self::IdentityTooLong => "identity_too_long",
            Self::ChainDepthExceeded => "chain_depth_exceeded",
            Self::IoError => "io_error",
            Self::NoFollowUnsupported => "no_follow_unsupported",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct DiscoveryOutcome {
    /// Accepted sources in rendering order (project root first), frozen
    /// post-redaction.
    pub sources: Vec<ManifestSource>,
    /// Ordered content-free diagnostics for everything omitted.
    pub diagnostics: Vec<ManifestDiagnostic>,
}

fn diagnostic(
    reason: DiagnosticReason,
    path: Option<String>,
    observed: Option<u64>,
) -> ManifestDiagnostic {
    ManifestDiagnostic {
        reason: reason.as_str().to_owned(),
        path,
        observed,
    }
}

/// Discover `EULER.md` sources for an already canonicalized workspace root.
/// Never fails: every problem becomes a typed diagnostic and startup
/// continues with whatever was safely admitted.
pub(crate) fn discover(canonical_workspace: &Path, redactor: &SecretRedactor) -> DiscoveryOutcome {
    imp::discover(canonical_workspace, redactor)
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::ffi::{CString, OsStr};
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    struct Candidate {
        rel_path: String,
        content: String,
    }

    pub(super) fn discover(
        canonical_workspace: &Path,
        redactor: &SecretRedactor,
    ) -> DiscoveryOutcome {
        let mut diagnostics = Vec::new();
        let root = find_discovery_root(canonical_workspace, &mut diagnostics);
        let candidates = collect_candidates(&root, canonical_workspace, redactor, &mut diagnostics);
        let sources = admit_candidates(candidates, &mut diagnostics);
        DiscoveryOutcome {
            sources,
            diagnostics,
        }
    }

    /// Nearest ancestor (workspace inclusive) whose directory listing has an
    /// exact `.git` entry that is a regular file or directory. The search is
    /// bounded by the chain-depth limit; without a marker inside the bound
    /// the chain is the workspace alone.
    fn find_discovery_root(
        canonical_workspace: &Path,
        diagnostics: &mut Vec<ManifestDiagnostic>,
    ) -> PathBuf {
        for (level, dir) in canonical_workspace.ancestors().enumerate() {
            if level >= MAX_CHAIN_LEVELS {
                diagnostics.push(diagnostic(
                    DiagnosticReason::ChainDepthExceeded,
                    None,
                    Some(MAX_CHAIN_LEVELS as u64),
                ));
                break;
            }
            if has_exact_git_marker(dir) {
                return dir.to_path_buf();
            }
        }
        canonical_workspace.to_path_buf()
    }

    /// Exact-entry `.git` marker check. Enumerates the directory (a plain
    /// path lookup on a case-insensitive filesystem could match `.GIT`) and
    /// classifies the entry without following symlinks.
    fn has_exact_git_marker(dir: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            if entry.file_name().as_os_str().as_bytes() != b".git" {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                return false;
            };
            // A symlinked `.git` is not a marker; a regular file marks a
            // linked worktree or submodule root (its contents are never
            // followed), and a directory marks an ordinary repository.
            return file_type.is_file() || file_type.is_dir();
        }
        false
    }

    fn collect_candidates(
        root: &Path,
        canonical_workspace: &Path,
        redactor: &SecretRedactor,
        diagnostics: &mut Vec<ManifestDiagnostic>,
    ) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        let rel = canonical_workspace
            .strip_prefix(root)
            .expect("discovery root is an ancestor of the workspace");
        let components: Vec<&OsStr> = rel
            .components()
            .map(|component| component.as_os_str())
            .collect();

        let mut dir = match open_dir_abs(root) {
            Ok(dir) => dir,
            Err(_) => {
                diagnostics.push(diagnostic(DiagnosticReason::IoError, None, None));
                return candidates;
            }
        };
        let mut rel_dir = String::new();

        for depth in 0..=components.len() {
            scan_dir(&dir, &rel_dir, redactor, &mut candidates, diagnostics);
            let Some(component) = components.get(depth) else {
                break;
            };
            let Some(component_str) = component.to_str() else {
                diagnostics.push(diagnostic(DiagnosticReason::NonUtf8Path, None, None));
                break;
            };
            let next_rel = join_rel(&rel_dir, component_str);
            dir = match open_child_dir(&dir, component) {
                Ok(next) => next,
                Err(error) => {
                    let reason = if error.raw_os_error() == Some(libc::ELOOP)
                        || error.raw_os_error() == Some(libc::ENOTDIR)
                    {
                        DiagnosticReason::SymlinkRejected
                    } else {
                        DiagnosticReason::IoError
                    };
                    diagnostics.push(diagnostic(reason, Some(next_rel), None));
                    break;
                }
            };
            rel_dir = next_rel;
        }
        candidates
    }

    /// Scan one held directory handle for an exact `EULER.md` entry and
    /// near-miss casings, then read the candidate under the stable-read
    /// protocol.
    fn scan_dir(
        dir: &OwnedFd,
        rel_dir: &str,
        redactor: &SecretRedactor,
        candidates: &mut Vec<Candidate>,
        diagnostics: &mut Vec<ManifestDiagnostic>,
    ) {
        let names = match dir_entry_names(dir) {
            Ok(names) => names,
            Err(_) => {
                diagnostics.push(diagnostic(
                    DiagnosticReason::IoError,
                    rel_identity(rel_dir),
                    None,
                ));
                return;
            }
        };
        let mut exact = false;
        for name in &names {
            if name.as_slice() == EULER_MD_FILE_NAME.as_bytes() {
                exact = true;
            } else if name.eq_ignore_ascii_case(EULER_MD_FILE_NAME.as_bytes()) {
                // Near-miss casing is diagnosed but never loaded.
                let path = std::str::from_utf8(name)
                    .ok()
                    .map(|name| join_rel(rel_dir, name));
                diagnostics.push(diagnostic(DiagnosticReason::CaseMismatch, path, None));
            }
        }
        if !exact {
            return;
        }
        let rel_path = join_rel(rel_dir, EULER_MD_FILE_NAME);
        if rel_path.len() > MAX_IDENTITY_BYTES {
            diagnostics.push(diagnostic(DiagnosticReason::IdentityTooLong, None, None));
            return;
        }
        match read_candidate_stable(dir) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => {
                    // Freeze the redacted bytes; the raw pre-redaction bytes
                    // are dropped here and can never reach a digest, event,
                    // diagnostic, or model input.
                    let content = redactor.redact(&text);
                    candidates.push(Candidate { rel_path, content });
                }
                Err(_) => {
                    diagnostics.push(diagnostic(
                        DiagnosticReason::InvalidUtf8,
                        Some(rel_path),
                        None,
                    ));
                }
            },
            Err(error) => {
                let (reason, observed) = match error {
                    ReadCandidateError::Symlink => (DiagnosticReason::SymlinkRejected, None),
                    ReadCandidateError::NotRegular => (DiagnosticReason::NotRegularFile, None),
                    ReadCandidateError::TooLarge(size) => {
                        (DiagnosticReason::SourceTooLarge, Some(size))
                    }
                    ReadCandidateError::Unstable => (DiagnosticReason::ChangedDuringRead, None),
                    ReadCandidateError::Io => (DiagnosticReason::IoError, None),
                };
                diagnostics.push(diagnostic(reason, Some(rel_path), observed));
            }
        }
    }

    /// Apply the source-count and combined-content bounds. More-specific
    /// sources (deeper in the chain) win admission priority; accepted sources
    /// keep root-first rendering order. Whole sources are omitted with
    /// diagnostics — content is never sliced.
    fn admit_candidates(
        candidates: Vec<Candidate>,
        diagnostics: &mut Vec<ManifestDiagnostic>,
    ) -> Vec<ManifestSource> {
        let mut selected = vec![false; candidates.len()];
        let mut accepted = 0usize;
        let mut combined = 0usize;
        for index in (0..candidates.len()).rev() {
            let candidate = &candidates[index];
            if accepted >= MAX_EULER_MD_SOURCES {
                diagnostics.push(diagnostic(
                    DiagnosticReason::SourceCountExceeded,
                    Some(candidate.rel_path.clone()),
                    None,
                ));
                continue;
            }
            if combined + candidate.content.len() > MAX_COMBINED_EULER_MD_BYTES {
                diagnostics.push(diagnostic(
                    DiagnosticReason::CombinedLimitExceeded,
                    Some(candidate.rel_path.clone()),
                    Some(candidate.content.len() as u64),
                ));
                continue;
            }
            accepted += 1;
            combined += candidate.content.len();
            selected[index] = true;
        }
        candidates
            .into_iter()
            .zip(selected)
            .filter_map(|(candidate, keep)| keep.then_some(candidate))
            .map(|candidate| ManifestSource {
                digest: source_digest_v1(&candidate.rel_path, &candidate.content),
                byte_len: candidate.content.len() as u64,
                path: candidate.rel_path,
                content: candidate.content,
            })
            .collect()
    }

    fn join_rel(rel_dir: &str, name: &str) -> String {
        if rel_dir.is_empty() {
            name.to_owned()
        } else {
            format!("{rel_dir}/{name}")
        }
    }

    fn rel_identity(rel_dir: &str) -> Option<String> {
        (!rel_dir.is_empty()).then(|| rel_dir.to_owned())
    }

    fn open_dir_abs(path: &Path) -> std::io::Result<OwnedFd> {
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn open_child_dir(parent: &OwnedFd, name: &OsStr) -> std::io::Result<OwnedFd> {
        openat(
            parent,
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    }

    fn openat(parent: &OwnedFd, name: &OsStr, flags: libc::c_int) -> std::io::Result<OwnedFd> {
        let c_name = CString::new(name.as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        let fd = unsafe { libc::openat(parent.as_raw_fd(), c_name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn dir_entry_names(dir: &OwnedFd) -> std::io::Result<Vec<Vec<u8>>> {
        let dup = unsafe { libc::dup(dir.as_raw_fd()) };
        if dup < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let dirp = unsafe { libc::fdopendir(dup) };
        if dirp.is_null() {
            let error = std::io::Error::last_os_error();
            unsafe { libc::close(dup) };
            return Err(error);
        }
        unsafe { libc::rewinddir(dirp) };
        let mut names = Vec::new();
        loop {
            let entry = unsafe { libc::readdir(dirp) };
            if entry.is_null() {
                break;
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            names.push(bytes.to_vec());
        }
        unsafe { libc::closedir(dirp) };
        // Deterministic selection must not depend on filesystem iteration
        // order.
        names.sort();
        Ok(names)
    }

    enum ReadCandidateError {
        Symlink,
        NotRegular,
        TooLarge(u64),
        Unstable,
        Io,
    }

    #[derive(Eq, PartialEq)]
    struct StatSignature {
        dev: u64,
        ino: u64,
        size: i64,
        mtime: (i64, i64),
        ctime: (i64, i64),
    }

    #[allow(clippy::unnecessary_cast)] // stat field widths differ per platform
    fn stat_signature(stat: &libc::stat) -> StatSignature {
        StatSignature {
            dev: stat.st_dev as u64,
            ino: stat.st_ino as u64,
            size: stat.st_size,
            mtime: (stat.st_mtime, stat.st_mtime_nsec as i64),
            ctime: (stat.st_ctime, stat.st_ctime_nsec as i64),
        }
    }

    fn fstat(fd: &impl AsRawFd) -> std::io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { stat.assume_init() })
    }

    /// One stable-read attempt from one freshly verified handle: open with
    /// no-follow (O_NONBLOCK so a FIFO cannot block startup), verify the
    /// opened handle is a bounded regular file, read, then compare stable
    /// metadata before and after the read.
    fn read_candidate_once(dir: &OwnedFd) -> Result<Vec<u8>, ReadCandidateError> {
        let fd = openat(
            dir,
            OsStr::new(EULER_MD_FILE_NAME),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
        .map_err(|error| match error.raw_os_error() {
            Some(libc::ELOOP) | Some(libc::EMLINK) => ReadCandidateError::Symlink,
            _ => ReadCandidateError::Io,
        })?;
        let before = fstat(&fd).map_err(|_| ReadCandidateError::Io)?;
        if before.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(ReadCandidateError::NotRegular);
        }
        if before.st_size < 0 || before.st_size as u64 > MAX_EULER_MD_BYTES as u64 {
            return Err(ReadCandidateError::TooLarge(before.st_size.max(0) as u64));
        }
        let mut file = std::fs::File::from(fd);
        let mut bytes = Vec::new();
        file.by_ref()
            .take(MAX_EULER_MD_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ReadCandidateError::Io)?;
        if bytes.len() > MAX_EULER_MD_BYTES {
            return Err(ReadCandidateError::TooLarge(bytes.len() as u64));
        }
        #[cfg(test)]
        super::test_hook::fire_after_read();
        let after = fstat(&file).map_err(|_| ReadCandidateError::Io)?;
        if stat_signature(&before) != stat_signature(&after) {
            return Err(ReadCandidateError::Unstable);
        }
        Ok(bytes)
    }

    /// Stable-read protocol: retry an unstable source at most once, then omit
    /// it with `changed_during_read`.
    fn read_candidate_stable(dir: &OwnedFd) -> Result<Vec<u8>, ReadCandidateError> {
        match read_candidate_once(dir) {
            Err(ReadCandidateError::Unstable) => match read_candidate_once(dir) {
                Err(ReadCandidateError::Unstable) => Err(ReadCandidateError::Unstable),
                other => other,
            },
            other => other,
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;

    /// This platform has no ratified no-follow read path; omit every source
    /// rather than follow a link (project-context contract, containment).
    pub(super) fn discover(
        _canonical_workspace: &Path,
        _redactor: &SecretRedactor,
    ) -> DiscoveryOutcome {
        DiscoveryOutcome {
            sources: Vec::new(),
            diagnostics: vec![diagnostic(
                DiagnosticReason::NoFollowUnsupported,
                None,
                None,
            )],
        }
    }
}

#[cfg(test)]
pub(crate) mod test_hook {
    use std::cell::RefCell;

    thread_local! {
        static AFTER_READ: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
    }

    /// Install a callback that runs after each stable-read attempt's bounded
    /// read and before its confirming metadata check. Used to exercise the
    /// concurrent-mutation path deterministically.
    pub(crate) fn set_after_read(hook: Option<Box<dyn FnMut()>>) {
        AFTER_READ.with(|cell| *cell.borrow_mut() = hook);
    }

    pub(super) fn fire_after_read() {
        AFTER_READ.with(|cell| {
            if let Some(hook) = cell.borrow_mut().as_mut() {
                hook();
            }
        });
    }
}
