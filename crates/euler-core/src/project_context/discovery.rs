//! Secure `EULER.md` discovery along the project-root-to-workspace chain.
//!
//! Containment rules (ADR 0017 / project-context contract):
//! - the workspace anchor is established by a component-wise `openat` walk
//!   from the filesystem root with `O_NOFOLLOW` on every step — never a
//!   check-then-open of an absolute path. The walked path is already
//!   canonical, so it contains no symlinks by construction; any race that
//!   swaps one in makes the walk fail closed with a typed diagnostic;
//! - the project discovery root is the nearest ancestor with an exact `.git`
//!   entry that is a regular file or directory; a symlinked `.git` is not a
//!   marker, and a `.git` file (linked worktree / submodule) is a root whose
//!   contents are never followed;
//! - every subsequent open is relative to a held directory handle with
//!   no-follow semantics; symlinks, reparse points, devices, FIFOs, and
//!   other non-regular files are rejected after the handle is verified;
//! - directory entries are compared by exact name — a case-insensitive
//!   filesystem lookup must never admit `euler.md` as `EULER.md` — and
//!   enumeration is bounded: a directory with more entries than the frozen
//!   per-level cap is omitted whole with a typed diagnostic, never scanned
//!   through a truncated listing;
//! - reads follow the stable-read protocol: each verification is a pair of
//!   bounded reads from independently verified handles that must be
//!   byte-identical (per-handle metadata comparison is a fast-path reject);
//!   one retry, then a typed `changed_during_read` omission;
//! - malformed, unsafe, or over-limit sources are omitted whole with typed
//!   content-free diagnostics, never truncated;
//! - discovery reads the working tree and ignores version-control state.
//!
//! A platform without no-follow reads omits every source with a
//! `no_follow_unsupported` diagnostic instead of following symlinks.

use super::digest::{skill_digest_v1, source_digest_v1};
use super::manifest::{
    validate_skill_description, validate_skill_name, ManifestDiagnostic, ManifestSkill,
    ManifestSource, SkillScope,
};
use super::{
    MAX_CHAIN_LEVELS, MAX_COMBINED_EULER_MD_BYTES, MAX_COMBINED_SKILL_BODY_BYTES, MAX_DIR_ENTRIES,
    MAX_EULER_MD_BYTES, MAX_EULER_MD_SOURCES, MAX_IDENTITY_BYTES, MAX_SKILLS,
    MAX_SKILL_DIRECTORIES, MAX_SKILL_FILE_BYTES, MAX_SKILL_TRAVERSAL_DEPTH,
};
use crate::redaction::SecretRedactor;
use std::path::Path;

pub(crate) const EULER_MD_FILE_NAME: &str = "EULER.md";
pub(crate) const SKILL_FILE_NAME: &str = "SKILL.md";

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
    /// A directory level had more entries than the frozen per-level cap.
    /// Its listing is unknowable, so the whole preflight fails closed
    /// rather than selecting over a truncated listing or letting the
    /// boundary search continue past it.
    DirEntriesExceeded,
    /// A level between the workspace and the nearest determinable `.git`
    /// marker could not be enumerated, so the repository boundary is
    /// unknowable. The whole preflight failed closed: an indeterminate
    /// level must never widen discovery upward across a possible nested
    /// repository boundary.
    MarkerIndeterminate,
    /// The bounded preflight itself produced more diagnostics than the
    /// manifest bound; the whole preflight collapsed to this single record
    /// and no source was admitted.
    DiagnosticOverflow,
    /// Defensive collapse: the preflight assembled a manifest that failed
    /// its own validation. Nothing was admitted.
    PreflightInvalid,
    IoError,
    SkillDepthExceeded,
    SkillDirectoryCountExceeded,
    SkillCountExceeded,
    SkillFrontmatterInvalid,
    SkillNameInvalid,
    SkillNameMismatch,
    SkillDescriptionInvalid,
    SkillNameAmbiguous,
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
            Self::DirEntriesExceeded => "dir_entries_exceeded",
            Self::MarkerIndeterminate => "marker_indeterminate",
            Self::DiagnosticOverflow => "diagnostic_overflow",
            Self::PreflightInvalid => "preflight_invalid",
            Self::IoError => "io_error",
            Self::SkillDepthExceeded => "skill_depth_exceeded",
            Self::SkillDirectoryCountExceeded => "skill_directory_count_exceeded",
            Self::SkillCountExceeded => "skill_count_exceeded",
            Self::SkillFrontmatterInvalid => "skill_frontmatter_invalid",
            Self::SkillNameInvalid => "skill_name_invalid",
            Self::SkillNameMismatch => "skill_name_mismatch",
            Self::SkillDescriptionInvalid => "skill_description_invalid",
            Self::SkillNameAmbiguous => "skill_name_ambiguous",
            Self::NoFollowUnsupported => "no_follow_unsupported",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct DiscoveryOutcome {
    /// Accepted sources in rendering order (project root first), frozen
    /// post-redaction.
    pub sources: Vec<ManifestSource>,
    /// Accepted user- and project-scope skills, sorted by normalized name.
    pub skills: Vec<ManifestSkill>,
    /// Ordered content-free diagnostics for everything omitted.
    pub diagnostics: Vec<ManifestDiagnostic>,
}

pub(crate) fn diagnostic(
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
pub(crate) fn discover_with_user_skills(
    canonical_workspace: &Path,
    user_skills_root: Option<&Path>,
    redactor: &SecretRedactor,
) -> DiscoveryOutcome {
    imp::discover(canonical_workspace, user_skills_root, redactor)
}

#[cfg(test)]
pub(crate) fn discover(canonical_workspace: &Path, redactor: &SecretRedactor) -> DiscoveryOutcome {
    discover_with_user_skills(canonical_workspace, None, redactor)
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::ffi::{CString, OsStr, OsString};
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;

    struct Candidate {
        rel_path: String,
        content: String,
    }

    struct SkillCandidate {
        name: String,
        description: String,
        scope: SkillScope,
        path: String,
        body: String,
    }

    #[derive(serde::Deserialize)]
    struct SkillFrontmatter {
        name: String,
        description: String,
        #[serde(flatten)]
        _inert: std::collections::BTreeMap<String, serde_yaml::Value>,
    }

    /// One directory on the anchored workspace chain.
    struct ChainDir {
        fd: OwnedFd,
        /// Component name of this directory (`None` for the filesystem
        /// root).
        name: Option<OsString>,
        /// The handle was opened traversal-only (search permission without
        /// read permission); enumeration must upgrade via `openat(fd, ".")`.
        traversal_only: bool,
        /// Bounded enumeration result, cached so marker search and source
        /// scanning observe one listing.
        entries: Option<Enumeration>,
    }

    enum Enumeration {
        Names(Vec<Vec<u8>>),
        CapExceeded(u64),
        Failed,
    }

    pub(super) fn discover(
        canonical_workspace: &Path,
        user_skills_root: Option<&Path>,
        redactor: &SecretRedactor,
    ) -> DiscoveryOutcome {
        let mut diagnostics = Vec::new();
        // Anchor: open every component of the canonical workspace path from
        // the filesystem root with no-follow semantics, retaining handles
        // for the marker-search window (the last MAX_CHAIN_LEVELS
        // directories, workspace inclusive).
        let (mut window, truncated_above) =
            match open_workspace_chain(canonical_workspace, &mut diagnostics) {
                Some(walk) => walk,
                None => {
                    return DiscoveryOutcome {
                        sources: Vec::new(),
                        skills: Vec::new(),
                        diagnostics,
                    }
                }
            };
        // Nearest marker inside the window (workspace upward). A level whose
        // bounded enumeration caps out or fails is INDETERMINATE: it could
        // hide a nested-repository `.git`, so continuing the search upward
        // would silently erase a repository boundary. An indeterminate level
        // must never widen discovery upward — the whole preflight fails
        // closed instead. (These records carry no path: no discovery root
        // exists yet to relativize against, and ancestor path fragments are
        // outside-workspace data the manifest must not carry.)
        let root_index = match find_marker_index(&mut window) {
            MarkerSearch::Indeterminate { observed } => {
                let companion = match observed {
                    Some(observed) => {
                        diagnostic(DiagnosticReason::DirEntriesExceeded, None, Some(observed))
                    }
                    None => diagnostic(DiagnosticReason::IoError, None, None),
                };
                diagnostics.push(companion);
                diagnostics.push(diagnostic(
                    DiagnosticReason::MarkerIndeterminate,
                    None,
                    observed,
                ));
                return DiscoveryOutcome {
                    sources: Vec::new(),
                    skills: Vec::new(),
                    diagnostics,
                };
            }
            MarkerSearch::NotFound => {
                if truncated_above {
                    diagnostics.push(diagnostic(
                        DiagnosticReason::ChainDepthExceeded,
                        None,
                        Some(MAX_CHAIN_LEVELS as u64),
                    ));
                }
                window.len() - 1
            }
            MarkerSearch::Found(index) => index,
        };
        let project_chain = &mut window[root_index..];
        let candidates = scan_chain(project_chain, redactor, &mut diagnostics);
        let sources = admit_candidates(candidates, &mut diagnostics);
        let skills = discover_skills(project_chain, user_skills_root, redactor, &mut diagnostics);
        DiscoveryOutcome {
            sources,
            skills,
            diagnostics,
        }
    }

    /// Component-wise `openat` walk from `/` to the canonical workspace.
    /// Every step uses `O_NOFOLLOW`, so a component swapped for a symlink
    /// after canonicalization fails the walk closed (typed diagnostic, no
    /// sources) instead of being followed. Returns the retained window (the
    /// deepest `MAX_CHAIN_LEVELS` directories) and whether directories were
    /// dropped above it.
    fn open_workspace_chain(
        canonical_workspace: &Path,
        diagnostics: &mut Vec<ManifestDiagnostic>,
    ) -> Option<(Vec<ChainDir>, bool)> {
        let mut components = canonical_workspace.components();
        if components.next() != Some(Component::RootDir) {
            // Discovery operates on canonicalized absolute Unix paths only.
            diagnostics.push(diagnostic(DiagnosticReason::IoError, None, None));
            return None;
        }
        let root_fd = match open_filesystem_root() {
            Ok(fd) => fd,
            Err(_) => {
                diagnostics.push(diagnostic(DiagnosticReason::IoError, None, None));
                return None;
            }
        };
        let mut window: Vec<ChainDir> = vec![ChainDir {
            fd: root_fd,
            name: None,
            traversal_only: false,
            entries: None,
        }];
        let mut truncated_above = false;
        for component in components {
            let Component::Normal(name) = component else {
                // A canonical path has no `.`/`..` components; anything else
                // means the input is not the canonical path this walk is
                // contracted to anchor.
                diagnostics.push(diagnostic(DiagnosticReason::IoError, None, None));
                return None;
            };
            let parent = &window.last().expect("window is never empty").fd;
            let next = match open_component_dir(parent, name) {
                Ok(next) => next,
                Err(error) => {
                    let reason = if error.raw_os_error() == Some(libc::ELOOP)
                        || error.raw_os_error() == Some(libc::ENOTDIR)
                    {
                        DiagnosticReason::SymlinkRejected
                    } else {
                        DiagnosticReason::IoError
                    };
                    diagnostics.push(diagnostic(reason, None, None));
                    return None;
                }
            };
            window.push(ChainDir {
                fd: next.fd,
                name: Some(name.to_owned()),
                traversal_only: next.traversal_only,
                entries: None,
            });
            if window.len() > MAX_CHAIN_LEVELS {
                window.remove(0);
                truncated_above = true;
            }
        }
        Some((window, truncated_above))
    }

    enum MarkerSearch {
        Found(usize),
        NotFound,
        /// A level between the workspace and the nearest determinable marker
        /// could not be fully enumerated (entry cap with the observed count,
        /// or an enumeration failure with `None`). The repository boundary
        /// is unknowable: the level's own `.git` presence and its contents
        /// are equally invisible, so it may neither be claimed as a marker
        /// nor skipped.
        Indeterminate {
            observed: Option<u64>,
        },
    }

    /// Index (within the window) of the nearest ancestor with an exact
    /// `.git` marker, searching from the workspace upward. The search stops
    /// at the first indeterminate level: continuing upward past a level
    /// whose listing is unknowable would let an over-cap (or unreadable)
    /// nested repository root leak the OUTER repository's guidance across
    /// the boundary.
    fn find_marker_index(window: &mut [ChainDir]) -> MarkerSearch {
        for index in (0..window.len()).rev() {
            ensure_enumerated(&mut window[index]);
            match window[index].entries.as_ref().expect("enumerated above") {
                Enumeration::CapExceeded(observed) => {
                    return MarkerSearch::Indeterminate {
                        observed: Some(*observed),
                    };
                }
                Enumeration::Failed => {
                    return MarkerSearch::Indeterminate { observed: None };
                }
                Enumeration::Names(_) => {}
            }
            let has_exact_git = matches!(
                &window[index].entries,
                Some(Enumeration::Names(names))
                    if names.iter().any(|name| name.as_slice() == b".git")
            );
            if !has_exact_git {
                continue;
            }
            // Classify the exact entry without following symlinks: a
            // symlinked `.git` is not a marker; a regular file marks a
            // linked worktree or submodule root (its contents are never
            // followed), and a directory marks an ordinary repository.
            let Ok(stat) = fstatat_nofollow(&window[index].fd, OsStr::new(".git")) else {
                continue;
            };
            let file_type = stat.st_mode & libc::S_IFMT;
            if file_type == libc::S_IFDIR || file_type == libc::S_IFREG {
                return MarkerSearch::Found(index);
            }
        }
        MarkerSearch::NotFound
    }

    /// Populate the cached bounded enumeration for one chain directory.
    fn ensure_enumerated(dir: &mut ChainDir) {
        if dir.entries.is_none() {
            dir.entries = Some(enumerate_bounded(&dir.fd, dir.traversal_only));
        }
    }

    /// Scan the chain (discovery root first, workspace last) for exact
    /// `EULER.md` candidates using the held handles and cached bounded
    /// listings.
    fn scan_chain(
        chain: &mut [ChainDir],
        redactor: &SecretRedactor,
        diagnostics: &mut Vec<ManifestDiagnostic>,
    ) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        let mut rel_dir = String::new();
        for (depth, dir) in chain.iter_mut().enumerate() {
            if depth > 0 {
                let name = dir
                    .name
                    .as_deref()
                    .expect("only the filesystem root has no component name");
                let Some(name) = name.to_str() else {
                    // Identities are normalized UTF-8; a non-UTF-8 component
                    // makes this level and everything beneath it
                    // unrepresentable.
                    diagnostics.push(diagnostic(DiagnosticReason::NonUtf8Path, None, None));
                    break;
                };
                rel_dir = join_rel(&rel_dir, name);
            }
            ensure_enumerated(dir);
            match dir.entries.as_ref().expect("enumerated above") {
                Enumeration::Failed => {
                    diagnostics.push(diagnostic(
                        DiagnosticReason::IoError,
                        rel_identity(&rel_dir),
                        None,
                    ));
                }
                Enumeration::CapExceeded(observed) => {
                    // Defensive: marker search already enumerated every
                    // chain level and fails the preflight closed on a cap,
                    // so this arm is unreachable in practice. Keep the
                    // typed whole-level omission as belt and braces.
                    diagnostics.push(diagnostic(
                        DiagnosticReason::DirEntriesExceeded,
                        rel_identity(&rel_dir),
                        Some(*observed),
                    ));
                }
                Enumeration::Names(names) => {
                    scan_dir_names(
                        &dir.fd,
                        names,
                        &rel_dir,
                        redactor,
                        &mut candidates,
                        diagnostics,
                    );
                }
            }
        }
        candidates
    }

    /// Scan one enumerated directory for an exact `EULER.md` entry and
    /// near-miss casings, then read the candidate under the stable-read
    /// protocol.
    fn scan_dir_names(
        dir: &OwnedFd,
        names: &[Vec<u8>],
        rel_dir: &str,
        redactor: &SecretRedactor,
        candidates: &mut Vec<Candidate>,
        diagnostics: &mut Vec<ManifestDiagnostic>,
    ) {
        let mut exact = false;
        for name in names {
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

    fn discover_skills(
        project_chain: &mut [ChainDir],
        user_skills_root: Option<&Path>,
        redactor: &SecretRedactor,
        diagnostics: &mut Vec<ManifestDiagnostic>,
    ) -> Vec<ManifestSkill> {
        let mut candidates = Vec::new();
        let mut directories_examined = 0usize;
        if let Some(root) = user_skills_root {
            scan_user_skills_root(
                root,
                redactor,
                diagnostics,
                &mut directories_examined,
                &mut candidates,
            );
        }
        let mut rel_dir = String::new();
        for (depth, dir) in project_chain.iter_mut().enumerate() {
            if depth > 0 {
                let Some(name) = dir.name.as_deref().and_then(OsStr::to_str) else {
                    diagnostics.push(diagnostic(DiagnosticReason::NonUtf8Path, None, None));
                    break;
                };
                rel_dir = join_rel(&rel_dir, name);
            }
            ensure_enumerated(dir);
            let Some(Enumeration::Names(names)) = dir.entries.as_ref() else {
                continue;
            };
            scan_project_skills_entry(
                &dir.fd,
                names,
                &rel_dir,
                redactor,
                diagnostics,
                &mut directories_examined,
                &mut candidates,
            );
        }
        admit_skill_candidates(candidates, diagnostics)
    }

    fn scan_user_skills_root(
        root: &Path,
        redactor: &SecretRedactor,
        diagnostics: &mut Vec<ManifestDiagnostic>,
        directories_examined: &mut usize,
        candidates: &mut Vec<SkillCandidate>,
    ) {
        if !root.exists() {
            return;
        }
        let Some(fd) = open_absolute_dir_nofollow(root) else {
            diagnostics.push(diagnostic(
                DiagnosticReason::SymlinkRejected,
                Some("user/skills".to_owned()),
                None,
            ));
            return;
        };
        scan_skills_tree(
            &fd,
            false,
            "user/skills",
            SkillScope::User,
            0,
            redactor,
            diagnostics,
            directories_examined,
            candidates,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_project_skills_entry(
        parent: &OwnedFd,
        names: &[Vec<u8>],
        rel_dir: &str,
        redactor: &SecretRedactor,
        diagnostics: &mut Vec<ManifestDiagnostic>,
        directories_examined: &mut usize,
        candidates: &mut Vec<SkillCandidate>,
    ) {
        if !names.iter().any(|name| name.as_slice() == b".euler") {
            return;
        }
        let identity = join_rel(rel_dir, ".euler/skills");
        let Ok(euler_dir) = open_component_dir(parent, OsStr::new(".euler")) else {
            diagnostics.push(diagnostic(
                DiagnosticReason::SymlinkRejected,
                Some(join_rel(rel_dir, ".euler")),
                None,
            ));
            return;
        };
        let Enumeration::Names(euler_names) =
            enumerate_bounded(&euler_dir.fd, euler_dir.traversal_only)
        else {
            diagnostics.push(diagnostic(DiagnosticReason::IoError, Some(identity), None));
            return;
        };
        if !euler_names.iter().any(|name| name.as_slice() == b"skills") {
            return;
        }
        let Ok(skills_dir) = open_component_dir(&euler_dir.fd, OsStr::new("skills")) else {
            diagnostics.push(diagnostic(
                DiagnosticReason::SymlinkRejected,
                Some(identity),
                None,
            ));
            return;
        };
        scan_skills_tree(
            &skills_dir.fd,
            skills_dir.traversal_only,
            &join_rel(rel_dir, ".euler/skills"),
            SkillScope::Project,
            0,
            redactor,
            diagnostics,
            directories_examined,
            candidates,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_skills_tree(
        dir: &OwnedFd,
        traversal_only: bool,
        identity: &str,
        scope: SkillScope,
        depth: usize,
        redactor: &SecretRedactor,
        diagnostics: &mut Vec<ManifestDiagnostic>,
        directories_examined: &mut usize,
        candidates: &mut Vec<SkillCandidate>,
    ) {
        if *directories_examined >= MAX_SKILL_DIRECTORIES {
            diagnostics.push(diagnostic(
                DiagnosticReason::SkillDirectoryCountExceeded,
                Some(identity.to_owned()),
                Some(*directories_examined as u64),
            ));
            return;
        }
        *directories_examined += 1;
        let enumeration = enumerate_bounded(dir, traversal_only);
        let Enumeration::Names(names) = enumeration else {
            diagnostics.push(diagnostic(
                DiagnosticReason::DirEntriesExceeded,
                Some(identity.to_owned()),
                None,
            ));
            return;
        };
        if names
            .iter()
            .any(|name| name.as_slice() == SKILL_FILE_NAME.as_bytes())
        {
            read_skill_candidate(dir, identity, scope, redactor, diagnostics, candidates);
        }
        if depth >= MAX_SKILL_TRAVERSAL_DEPTH {
            let has_child_directory = names.iter().any(|name| {
                fstatat_nofollow(dir, OsStr::from_bytes(name))
                    .is_ok_and(|stat| stat.st_mode & libc::S_IFMT == libc::S_IFDIR)
            });
            if has_child_directory {
                diagnostics.push(diagnostic(
                    DiagnosticReason::SkillDepthExceeded,
                    Some(identity.to_owned()),
                    Some(depth as u64),
                ));
            }
            return;
        }
        for name in names {
            if name == b"." || name == b".." || name.as_slice() == SKILL_FILE_NAME.as_bytes() {
                continue;
            }
            let Some(name_text) = std::str::from_utf8(&name).ok() else {
                diagnostics.push(diagnostic(DiagnosticReason::NonUtf8Path, None, None));
                continue;
            };
            let Ok(stat) = fstatat_nofollow(dir, OsStr::new(name_text)) else {
                continue;
            };
            if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
                continue;
            }
            let child_identity = join_rel(identity, name_text);
            match open_component_dir(dir, OsStr::new(name_text)) {
                Ok(child) => scan_skills_tree(
                    &child.fd,
                    child.traversal_only,
                    &child_identity,
                    scope,
                    depth + 1,
                    redactor,
                    diagnostics,
                    directories_examined,
                    candidates,
                ),
                Err(_) => diagnostics.push(diagnostic(
                    DiagnosticReason::SymlinkRejected,
                    Some(child_identity),
                    None,
                )),
            }
        }
    }

    fn read_skill_candidate(
        dir: &OwnedFd,
        directory_identity: &str,
        scope: SkillScope,
        redactor: &SecretRedactor,
        diagnostics: &mut Vec<ManifestDiagnostic>,
        candidates: &mut Vec<SkillCandidate>,
    ) {
        let path = join_rel(directory_identity, SKILL_FILE_NAME);
        match read_named_candidate_stable(dir, SKILL_FILE_NAME, MAX_SKILL_FILE_BYTES) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => match parse_skill_package(&text, directory_identity, scope, redactor) {
                    Ok(candidate) => candidates.push(candidate),
                    Err(reason) => diagnostics.push(diagnostic(reason, Some(path), None)),
                },
                Err(_) => {
                    diagnostics.push(diagnostic(DiagnosticReason::InvalidUtf8, Some(path), None))
                }
            },
            Err(error) => {
                let (reason, observed) = read_error_diagnostic(error);
                diagnostics.push(diagnostic(reason, Some(path), observed));
            }
        }
    }

    fn parse_skill_package(
        text: &str,
        directory_identity: &str,
        scope: SkillScope,
        redactor: &SecretRedactor,
    ) -> Result<SkillCandidate, DiagnosticReason> {
        let normalized = text.strip_prefix('\u{feff}').unwrap_or(text);
        let Some(rest) = normalized.strip_prefix("---\n") else {
            return Err(DiagnosticReason::SkillFrontmatterInvalid);
        };
        let Some((frontmatter, body)) = rest.split_once("\n---\n") else {
            return Err(DiagnosticReason::SkillFrontmatterInvalid);
        };
        let parsed: SkillFrontmatter = serde_yaml::from_str(frontmatter)
            .map_err(|_| DiagnosticReason::SkillFrontmatterInvalid)?;
        validate_skill_name(&parsed.name).map_err(|_| DiagnosticReason::SkillNameInvalid)?;
        validate_skill_description(&parsed.description)
            .map_err(|_| DiagnosticReason::SkillDescriptionInvalid)?;
        let basename = directory_identity.rsplit('/').next().unwrap_or("");
        if basename != parsed.name {
            return Err(DiagnosticReason::SkillNameMismatch);
        }
        let body = redactor.redact(body);
        if body.len() > super::super::MAX_SKILL_BODY_BYTES {
            return Err(DiagnosticReason::SourceTooLarge);
        }
        Ok(SkillCandidate {
            name: parsed.name,
            description: redactor.redact(&parsed.description),
            scope,
            path: join_rel(directory_identity, SKILL_FILE_NAME),
            body,
        })
    }

    fn read_error_diagnostic(error: ReadCandidateError) -> (DiagnosticReason, Option<u64>) {
        match error {
            ReadCandidateError::Symlink => (DiagnosticReason::SymlinkRejected, None),
            ReadCandidateError::NotRegular => (DiagnosticReason::NotRegularFile, None),
            ReadCandidateError::TooLarge(size) => (DiagnosticReason::SourceTooLarge, Some(size)),
            ReadCandidateError::Unstable => (DiagnosticReason::ChangedDuringRead, None),
            ReadCandidateError::Io => (DiagnosticReason::IoError, None),
        }
    }

    fn admit_skill_candidates(
        mut candidates: Vec<SkillCandidate>,
        diagnostics: &mut Vec<ManifestDiagnostic>,
    ) -> Vec<ManifestSkill> {
        candidates.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then(left.scope.cmp(&right.scope))
                .then(left.path.cmp(&right.path))
        });
        let mut admitted = Vec::new();
        let mut combined = 0usize;
        let mut index = 0usize;
        while index < candidates.len() {
            let end = candidates[index..]
                .iter()
                .position(|candidate| candidate.name != candidates[index].name)
                .map_or(candidates.len(), |offset| index + offset);
            if end - index > 1 {
                for candidate in &candidates[index..end] {
                    diagnostics.push(diagnostic(
                        DiagnosticReason::SkillNameAmbiguous,
                        Some(candidate.path.clone()),
                        None,
                    ));
                }
                index = end;
                continue;
            }
            let candidate = &candidates[index];
            if admitted.len() >= MAX_SKILLS {
                diagnostics.push(diagnostic(
                    DiagnosticReason::SkillCountExceeded,
                    Some(candidate.path.clone()),
                    None,
                ));
            } else if combined.saturating_add(candidate.body.len()) > MAX_COMBINED_SKILL_BODY_BYTES
            {
                diagnostics.push(diagnostic(
                    DiagnosticReason::CombinedLimitExceeded,
                    Some(candidate.path.clone()),
                    Some(candidate.body.len() as u64),
                ));
            } else {
                combined += candidate.body.len();
                admitted.push(ManifestSkill {
                    name: candidate.name.clone(),
                    description: candidate.description.clone(),
                    scope: candidate.scope,
                    path: candidate.path.clone(),
                    body_len: candidate.body.len() as u64,
                    body_digest: skill_digest_v1(
                        candidate.scope,
                        &candidate.name,
                        &candidate.path,
                        &candidate.body,
                    ),
                    body: candidate.body.clone(),
                });
            }
            index = end;
        }
        admitted
    }

    fn open_absolute_dir_nofollow(path: &Path) -> Option<OwnedFd> {
        let canonical = path.canonicalize().ok()?;
        if canonical != path {
            return None;
        }
        let mut components = canonical.components();
        if components.next() != Some(Component::RootDir) {
            return None;
        }
        let mut current = open_filesystem_root().ok()?;
        for component in components {
            let Component::Normal(name) = component else {
                return None;
            };
            current = open_component_dir(&current, name).ok()?.fd;
        }
        Some(current)
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

    /// The filesystem root cannot be a symlink; it is the sole absolute-path
    /// open in this module.
    fn open_filesystem_root() -> std::io::Result<OwnedFd> {
        let c_path = CString::new("/").expect("no interior NUL");
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

    struct OpenedDir {
        fd: OwnedFd,
        traversal_only: bool,
    }

    /// Open one path component relative to its held parent handle with
    /// no-follow semantics. Ancestors that grant search permission but not
    /// read permission are opened traversal-only so the walk matches what
    /// the kernel itself would traverse.
    fn open_component_dir(parent: &OwnedFd, name: &OsStr) -> std::io::Result<OpenedDir> {
        let readable_flags =
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        match openat(parent, name, readable_flags) {
            Ok(fd) => Ok(OpenedDir {
                fd,
                traversal_only: false,
            }),
            Err(error)
                if error.raw_os_error() == Some(libc::EACCES)
                    || error.raw_os_error() == Some(libc::EPERM) =>
            {
                let fd = openat(parent, name, traversal_open_flags())?;
                Ok(OpenedDir {
                    fd,
                    traversal_only: true,
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Search-only directory open flags for ancestors without read
    /// permission: `O_PATH` on Linux, `O_EXEC` (POSIX `O_SEARCH`) on macOS.
    #[cfg(target_os = "linux")]
    fn traversal_open_flags() -> libc::c_int {
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
    }

    #[cfg(not(target_os = "linux"))]
    fn traversal_open_flags() -> libc::c_int {
        libc::O_EXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
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

    fn fstatat_nofollow(dir: &OwnedFd, name: &OsStr) -> std::io::Result<libc::stat> {
        let c_name = CString::new(name.as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                c_name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { stat.assume_init() })
    }

    /// Bounded directory enumeration from a held handle. Stops as soon as
    /// the per-level entry cap is exceeded; callers must treat a capped
    /// level as unscannable rather than select over a truncated listing.
    fn enumerate_bounded(dir: &OwnedFd, traversal_only: bool) -> Enumeration {
        // Traversal-only handles cannot be read; upgrade through the handle
        // itself (`.` cannot be a symlink) so no path re-resolution occurs.
        let readable;
        let read_fd: &OwnedFd = if traversal_only {
            match openat(
                dir,
                OsStr::new("."),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            ) {
                Ok(fd) => {
                    readable = fd;
                    &readable
                }
                Err(_) => return Enumeration::Failed,
            }
        } else {
            dir
        };
        let dup = unsafe { libc::dup(read_fd.as_raw_fd()) };
        if dup < 0 {
            return Enumeration::Failed;
        }
        let dirp = unsafe { libc::fdopendir(dup) };
        if dirp.is_null() {
            unsafe { libc::close(dup) };
            return Enumeration::Failed;
        }
        unsafe { libc::rewinddir(dirp) };
        let mut names = Vec::new();
        let mut capped = false;
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
            if names.len() >= MAX_DIR_ENTRIES {
                capped = true;
                break;
            }
            names.push(bytes.to_vec());
        }
        unsafe { libc::closedir(dirp) };
        if capped {
            // Observed count: the cap plus the entry that proved the excess.
            return Enumeration::CapExceeded(MAX_DIR_ENTRIES as u64 + 1);
        }
        // Deterministic selection must not depend on filesystem iteration
        // order.
        names.sort();
        Enumeration::Names(names)
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

    /// One bounded read from one freshly verified handle: open with
    /// no-follow (O_NONBLOCK so a FIFO cannot block startup), verify the
    /// opened handle is a bounded regular file, read, then compare stable
    /// metadata before and after the read as a fast-path instability reject.
    fn read_named_candidate_once(
        dir: &OwnedFd,
        file_name: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, ReadCandidateError> {
        let fd = openat(
            dir,
            OsStr::new(file_name),
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
        if before.st_size < 0 || before.st_size as u64 > max_bytes as u64 {
            return Err(ReadCandidateError::TooLarge(before.st_size.max(0) as u64));
        }
        let mut file = std::fs::File::from(fd);
        let mut bytes = Vec::new();
        file.by_ref()
            .take(max_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ReadCandidateError::Io)?;
        if bytes.len() > max_bytes {
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

    /// One verification: two independent bounded reads whose bytes must be
    /// identical. Metadata granularity on some filesystems cannot detect a
    /// rapid same-size rewrite, so byte equality across two verified handles
    /// is the admission criterion; the per-handle metadata comparison is
    /// only a fast-path reject.
    fn read_named_candidate_verified(
        dir: &OwnedFd,
        file_name: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, ReadCandidateError> {
        let first = read_named_candidate_once(dir, file_name, max_bytes)?;
        let second = read_named_candidate_once(dir, file_name, max_bytes)?;
        if first == second {
            Ok(second)
        } else {
            Err(ReadCandidateError::Unstable)
        }
    }

    /// Stable-read protocol: retry an unstable source at most once, then
    /// omit it with `changed_during_read`. Errors other than instability
    /// abort immediately.
    fn read_named_candidate_stable(
        dir: &OwnedFd,
        file_name: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, ReadCandidateError> {
        match read_named_candidate_verified(dir, file_name, max_bytes) {
            Err(ReadCandidateError::Unstable) => {
                match read_named_candidate_verified(dir, file_name, max_bytes) {
                    Err(ReadCandidateError::Unstable) => Err(ReadCandidateError::Unstable),
                    other => other,
                }
            }
            other => other,
        }
    }

    fn read_candidate_stable(dir: &OwnedFd) -> Result<Vec<u8>, ReadCandidateError> {
        read_named_candidate_stable(dir, EULER_MD_FILE_NAME, MAX_EULER_MD_BYTES)
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;

    /// This platform has no ratified no-follow read path; omit every source
    /// rather than follow a link (project-context contract, containment).
    pub(super) fn discover(
        _canonical_workspace: &Path,
        _user_skills_root: Option<&Path>,
        _redactor: &SecretRedactor,
    ) -> DiscoveryOutcome {
        DiscoveryOutcome {
            sources: Vec::new(),
            skills: Vec::new(),
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

    /// Install a callback that runs after each bounded read and before its
    /// confirming metadata check. Used to exercise the concurrent-mutation
    /// path deterministically.
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
