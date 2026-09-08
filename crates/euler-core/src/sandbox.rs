//! Workspace subprocess sandboxing with Linux Bubblewrap and macOS Seatbelt.
//!
//! Linux gets a private runtime and no host home or network. The first
//! Seatbelt unit on macOS preserves broad reads for tool compatibility while
//! restricting writes and network; Unit 3 owns read-surface parity. A sandbox
//! is an execution boundary, not a synonym for permission approval.
//!
//! Residual: a Cargo `config.toml` in a reachable toolchain home may itself
//! declare a registry token. That file is not masked — it carries the registry
//! sources and build settings a build needs, and breaking the build to hide a
//! token the user can move is strictness the user would feel. The profile
//! detects it and says so once at session start instead.
//!
//! Bubblewrap is the default and enforced backend on Linux (ADR 0021 row A′),
//! and Seatbelt is the default and enforced backend on macOS (ADR 0021 row A).
//! Other platforms run on the host under the ordinary permission decider.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The first Linux profile Euler intends to advertise to users.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxProfile {
    /// A writable workspace with no host home or network namespace access.
    WorkspaceNoNetwork,
}

impl SandboxProfile {
    pub const fn label(self) -> &'static str {
        match self {
            Self::WorkspaceNoNetwork => "sandboxed workspace (network disabled)",
        }
    }
}

/// The execution boundary that actually ran, or would run, an agent command.
///
/// This is the seam the macOS Seatbelt backend slots into: call sites record
/// and branch on the backend, never on the host operating system.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxBackend {
    /// Linux Bubblewrap, the default and enforced backend.
    Bwrap,
    /// macOS Seatbelt, the default and enforced backend.
    Seatbelt,
    /// Direct host execution, gated only by the permission decider.
    Host,
}

impl SandboxBackend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bwrap => "bwrap",
            Self::Seatbelt => "seatbelt",
            Self::Host => "host",
        }
    }
}

/// Whether agent-controlled subprocesses use a sandbox profile.
///
/// This is a core execution choice, intentionally separate from the
/// capability gate and its approval modes. Linux and macOS default to the
/// enforced no-network profile; unsupported platforms run on the host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubprocessSandbox {
    /// Agent subprocesses run directly on the host. This is not "no
    /// confinement chosen": it is the honest state of a platform with no
    /// backend, and it is recorded as such.
    Host,
    Enforce(SandboxProfile),
}

impl Default for SubprocessSandbox {
    fn default() -> Self {
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            Self::Enforce(SandboxProfile::WorkspaceNoNetwork)
        } else {
            Self::Host
        }
    }
}

/// A concise, non-secret reason why a requested sandbox profile cannot run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxUnavailableReason {
    /// Euler is not running on a host with an implemented backend.
    UnsupportedPlatform,
    /// The `bwrap` executable was not found.
    BubblewrapMissing,
    /// The macOS Seatbelt launcher was not found at its trusted system path.
    SeatbeltMissing,
    /// A symlinked `.git` path cannot be protected by the first Seatbelt unit.
    GitMetadataSymlink,
    /// The platform launcher could not create the profile Euler requires.
    CannotEnforce,
    /// The selected workspace cannot be resolved to a directory.
    InvalidWorkspace,
}

impl SandboxUnavailableReason {
    pub const fn message(self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => {
                "workspace sandbox is currently supported on Linux and macOS only"
            }
            Self::BubblewrapMissing => {
                "workspace sandbox requires Bubblewrap (`bwrap`) to be installed"
            }
            Self::SeatbeltMissing => {
                "workspace sandbox requires macOS Seatbelt at /usr/bin/sandbox-exec"
            }
            Self::GitMetadataSymlink => {
                "workspace sandbox cannot safely protect a symbolic-link `.git` path"
            }
            Self::CannotEnforce => {
                "this host cannot enforce Euler's required workspace sandbox profile"
            }
            Self::InvalidWorkspace => {
                "workspace sandbox requires an accessible workspace directory"
            }
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::BubblewrapMissing => "bubblewrap_missing",
            Self::SeatbeltMissing => "seatbelt_missing",
            Self::GitMetadataSymlink => "git_metadata_symlink",
            Self::CannotEnforce => "cannot_enforce",
            Self::InvalidWorkspace => "invalid_workspace",
        }
    }
}

impl fmt::Display for SandboxUnavailableReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

/// The most likely host cause of a failed Bubblewrap probe. Bubblewrap's own
/// diagnostics are host-revealing and frequently unhelpful ("No permissions to
/// create new namespace"), so Euler names the cause it can actually verify.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxFailureCause {
    /// `bwrap` was not found at a trusted absolute path.
    BubblewrapMissing,
    /// `/usr/bin/sandbox-exec` is absent or is not an executable file.
    SeatbeltMissing,
    /// The workspace `.git` entry is a symbolic link.
    GitMetadataSymlink,
    /// A sysctl disables unprivileged user namespaces outright.
    UserNamespacesDisabled,
    /// Ubuntu 23.10+ AppArmor restricts unprivileged user namespaces.
    AppArmorUserNamespaceRestriction,
    /// The process is inside a container that does not permit nesting.
    Container,
    /// WSL1 has no user namespace support at all.
    Wsl1,
    /// `bwrap` predates a flag the profile requires.
    BubblewrapTooOld,
    /// Namespaces work, but this host would not give the profile its mounts.
    ProfileMountsRejected,
    /// Seatbelt exists, but macOS refused to apply Euler's required profile.
    SeatbeltProfileRejected,
    /// The probe did not finish in time, so nothing about it was learned.
    ProbeTimedOut,
    /// The workspace root is not a directory Euler can resolve.
    InvalidWorkspace,
    /// This platform has no sandbox backend at all.
    UnsupportedPlatform,
    /// Bubblewrap ran and failed for a reason Euler could not attribute.
    Unattributed,
}

impl SandboxFailureCause {
    /// The likely cause, in the user's terms.
    pub const fn description(self) -> &'static str {
        match self {
            Self::BubblewrapMissing => "`bwrap` is not installed at /usr/bin/bwrap or /bin/bwrap",
            Self::SeatbeltMissing => "macOS Seatbelt is not available at /usr/bin/sandbox-exec",
            Self::GitMetadataSymlink => {
                "the workspace `.git` entry is a symbolic link whose protected ancestors cannot be fixed in the static Seatbelt profile"
            }
            Self::UserNamespacesDisabled => {
                "unprivileged user namespaces are disabled by a kernel sysctl"
            }
            Self::AppArmorUserNamespaceRestriction => {
                "AppArmor restricts unprivileged user namespaces (Ubuntu 23.10 and later)"
            }
            Self::Container => {
                "this process is inside a container that does not allow nested user namespaces"
            }
            Self::Wsl1 => "WSL1 has no user namespace support",
            Self::BubblewrapTooOld => {
                "the installed Bubblewrap is older than 0.8.0 and cannot enforce this profile"
            }
            Self::ProfileMountsRejected => {
                "Bubblewrap can create namespaces on this host, but could not set up the \
profile's mounts for this workspace"
            }
            Self::SeatbeltProfileRejected => {
                "macOS refused to apply Euler's required Seatbelt profile"
            }
            Self::ProbeTimedOut => "the sandbox probe did not finish in time",
            Self::InvalidWorkspace => "the workspace root is not an accessible directory",
            Self::UnsupportedPlatform => "this platform has no sandbox backend yet",
            Self::Unattributed => "Bubblewrap could not create a user namespace",
        }
    }

    /// The cause implied by a reason on its own, for a failure that was not
    /// classified by a probe.
    ///
    /// `CannotEnforce` is the ambiguous one. When the backend probe already
    /// created a namespace, the profile's failure is about its mounts, and
    /// sending the user to namespace sysctls that demonstrably work is the
    /// misdiagnosis the two probes exist to avoid.
    pub fn for_reason(reason: SandboxUnavailableReason) -> Self {
        match reason {
            SandboxUnavailableReason::BubblewrapMissing => Self::BubblewrapMissing,
            SandboxUnavailableReason::SeatbeltMissing => Self::SeatbeltMissing,
            SandboxUnavailableReason::GitMetadataSymlink => Self::GitMetadataSymlink,
            SandboxUnavailableReason::InvalidWorkspace => Self::InvalidWorkspace,
            SandboxUnavailableReason::UnsupportedPlatform => Self::UnsupportedPlatform,
            SandboxUnavailableReason::CannotEnforce => match probe_sandbox_backend() {
                // Namespaces demonstrably work, so the profile's own mounts
                // are what this host rejected.
                SandboxStatus::Enforced(SandboxBackend::Bwrap) => Self::ProfileMountsRejected,
                SandboxStatus::Enforced(SandboxBackend::Seatbelt) => Self::SeatbeltProfileRejected,
                SandboxStatus::Enforced(SandboxBackend::Host) => Self::UnsupportedPlatform,
                // The backend probe already attributed this host's failure.
                // Re-deriving would discard a better answer: an out-of-date
                // Bubblewrap would become a lecture about namespace sysctls.
                SandboxStatus::Unavailable { cause, .. } => cause,
                SandboxStatus::Host => Self::UnsupportedPlatform,
            },
        }
    }

    /// The host change that would make the sandbox work.
    pub const fn remedy(self) -> &'static str {
        match self {
            Self::BubblewrapMissing => "install it (Debian/Ubuntu: `sudo apt install bubblewrap`)",
            Self::SeatbeltMissing => {
                "use a supported macOS installation that provides /usr/bin/sandbox-exec"
            }
            Self::GitMetadataSymlink => {
                "replace the `.git` symlink with a Git directory or standard `gitdir:` worktree pointer file"
            }
            Self::UserNamespacesDisabled => {
                "enable them: `sudo sysctl -w kernel.unprivileged_userns_clone=1` \
and `sudo sysctl -w user.max_user_namespaces=15000`"
            }
            Self::AppArmorUserNamespaceRestriction => {
                "allow them: `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`"
            }
            Self::Container => {
                "run the container with `--privileged`, or with a seccomp profile that permits \
`unshare(CLONE_NEWUSER)`"
            }
            Self::Wsl1 => "use WSL2 (`wsl --set-version <distro> 2`)",
            Self::BubblewrapTooOld => {
                "install bubblewrap 0.8.0 or newer, or use the bundled build tracked in \
https://github.com/2x11-xyz/euler/issues/230"
            }
            Self::ProfileMountsRejected => {
                "check that /usr, /etc and the workspace are readable and that the workspace \
is not on a filesystem Bubblewrap cannot bind, such as an unusual FUSE mount"
            }
            Self::SeatbeltProfileRejected => {
                "run Euler outside another macOS application sandbox; if the host is managed, ask the administrator to permit nested Seatbelt profiles"
            }
            Self::ProbeTimedOut => {
                "try again on a less loaded machine; if it persists, run the platform launcher \
directly (`bwrap` on Linux or `/usr/bin/sandbox-exec` on macOS) to see where it stops"
            }
            Self::InvalidWorkspace => {
                "start Euler in a directory that exists and that you can read"
            }
            Self::UnsupportedPlatform => {
                "use Linux or macOS, or explicitly select host execution when that profile ships"
            }
            Self::Unattributed => {
                "check `sysctl kernel.unprivileged_userns_clone user.max_user_namespaces` and \
run `bwrap --unshare-user --unshare-net --ro-bind / / /bin/true` by hand"
            }
        }
    }
}

impl fmt::Display for SandboxFailureCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.description())
    }
}

/// The session-start record of which execution boundary agent subprocesses
/// get. It is provenance, not a decision: `Unavailable` fails sandbox-requiring
/// commands closed rather than falling back to the host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxStatus {
    /// The named backend ran a trivial sandboxed command successfully.
    Enforced(SandboxBackend),
    /// No backend exists for this platform yet; commands run on the host under
    /// the permission decider.
    Host,
    /// A supported host whose required backend is unusable. Commands fail.
    Unavailable {
        reason: SandboxUnavailableReason,
        cause: SandboxFailureCause,
    },
}

impl SandboxStatus {
    /// The value recorded as `sandbox_backend` on `session.start`.
    pub const fn backend_label(self) -> &'static str {
        match self {
            Self::Enforced(backend) => backend.as_str(),
            Self::Host => SandboxBackend::Host.as_str(),
            Self::Unavailable { .. } => "unavailable",
        }
    }

    pub const fn reason(self) -> Option<SandboxUnavailableReason> {
        match self {
            Self::Enforced(_) | Self::Host => None,
            Self::Unavailable { reason, .. } => Some(reason),
        }
    }

    /// An operator-facing diagnostic naming the likely cause and the way out.
    pub fn diagnostic(self) -> Option<String> {
        let Self::Unavailable { cause, .. } = self else {
            return None;
        };
        Some(format!(
            "Euler could not start its subprocess sandbox: {}.\nTo fix it: {}.\n\
Until then `run_shell` and the `git_*` tools fail closed; there is no automatic \
fallback to host execution. The probe result is cached for this process, so \
fixing the host takes effect in a new run, not this one.",
            cause.description(),
            cause.remedy(),
        ))
    }
}

/// The result of probing the profile rather than merely locating a binary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxAvailability {
    Enforced(SandboxProfile),
    Unavailable(SandboxUnavailableReason),
}

impl SandboxAvailability {
    pub const fn is_enforced(self) -> bool {
        matches!(self, Self::Enforced(_))
    }
}

#[derive(Debug)]
enum SandboxLauncher {
    Bubblewrap(PathBuf),
    Seatbelt {
        executable: PathBuf,
        _scratch: tempfile::TempDir,
        scratch_path: PathBuf,
        git_metadata: PathBuf,
        git_metadata_resolved: PathBuf,
    },
}

impl SandboxLauncher {
    const fn backend(&self) -> SandboxBackend {
        match self {
            Self::Bubblewrap(_) => SandboxBackend::Bwrap,
            Self::Seatbelt { .. } => SandboxBackend::Seatbelt,
        }
    }
}

/// A workspace-specific profile that has already been probed. It retains the
/// stable availability result so callers can fail closed without copying raw
/// launcher diagnostics into tool output or provenance.
#[derive(Debug)]
pub(crate) struct WorkspaceSandbox {
    workspace: Option<PathBuf>,
    runtime: RuntimeRoots,
    backend: SandboxBackend,
    launcher: Option<SandboxLauncher>,
    availability: SandboxAvailability,
    /// How the profile probe ended, when one ran. `None` means the profile
    /// was ruled out before any process started.
    probe: Option<ProbeOutcome>,
}

impl WorkspaceSandbox {
    /// Build and probe a profile for one workspace. Construction itself never
    /// falls back to host execution: callers must inspect or propagate the
    /// resulting [`SandboxAvailability`].
    pub(crate) fn new(workspace: impl AsRef<Path>, profile: SandboxProfile) -> Self {
        Self::with_runtime_roots(workspace, profile, RuntimeRoots::detect())
    }

    fn with_runtime_roots(
        workspace: impl AsRef<Path>,
        profile: SandboxProfile,
        runtime: RuntimeRoots,
    ) -> Self {
        let backend = if cfg!(target_os = "linux") {
            SandboxBackend::Bwrap
        } else if cfg!(target_os = "macos") {
            SandboxBackend::Seatbelt
        } else {
            SandboxBackend::Host
        };
        if backend == SandboxBackend::Host {
            return Self::unavailable(
                None,
                runtime,
                backend,
                SandboxUnavailableReason::UnsupportedPlatform,
            );
        }
        let Ok(workspace) = canonical_workspace(workspace.as_ref()) else {
            return Self::unavailable(
                None,
                runtime,
                backend,
                SandboxUnavailableReason::InvalidWorkspace,
            );
        };
        let runtime = runtime.excluding(&workspace);
        match backend {
            SandboxBackend::Bwrap => Self::with_bwrap(workspace, runtime, profile),
            SandboxBackend::Seatbelt => Self::with_seatbelt(workspace, runtime, profile),
            SandboxBackend::Host => unreachable!("host returned above"),
        }
    }

    fn with_bwrap(workspace: PathBuf, runtime: RuntimeRoots, profile: SandboxProfile) -> Self {
        let backend = SandboxBackend::Bwrap;
        let Some(bwrap) = bwrap_path() else {
            return Self::unavailable(
                Some(workspace),
                runtime,
                backend,
                SandboxUnavailableReason::BubblewrapMissing,
            );
        };
        let (availability, outcome) = probe_bwrap_profile(&bwrap, &workspace, &runtime, profile);
        Self {
            workspace: Some(workspace),
            runtime,
            backend,
            launcher: Some(SandboxLauncher::Bubblewrap(bwrap)),
            availability,
            probe: Some(outcome),
        }
    }

    fn with_seatbelt(workspace: PathBuf, runtime: RuntimeRoots, profile: SandboxProfile) -> Self {
        let backend = SandboxBackend::Seatbelt;
        let Some(executable) = seatbelt_path() else {
            return Self::unavailable(
                Some(workspace),
                runtime,
                backend,
                SandboxUnavailableReason::SeatbeltMissing,
            );
        };
        let Some((scratch, scratch_path)) = seatbelt_scratch() else {
            return Self::unavailable(
                Some(workspace),
                runtime,
                backend,
                SandboxUnavailableReason::CannotEnforce,
            );
        };
        let git_metadata = workspace.join(".git");
        let git_metadata_resolved = match seatbelt_git_metadata_path(&git_metadata) {
            Ok(path) => path,
            Err(reason) => return Self::unavailable(Some(workspace), runtime, backend, reason),
        };
        let launcher = SandboxLauncher::Seatbelt {
            executable,
            _scratch: scratch,
            scratch_path,
            git_metadata,
            git_metadata_resolved,
        };
        let outcome = probe_seatbelt_profile(&launcher, &workspace, &runtime, profile);
        let availability = if outcome.succeeded() {
            SandboxAvailability::Enforced(profile)
        } else {
            SandboxAvailability::Unavailable(SandboxUnavailableReason::CannotEnforce)
        };
        Self {
            workspace: Some(workspace),
            runtime,
            backend,
            launcher: Some(launcher),
            availability,
            probe: Some(outcome),
        }
    }

    fn unavailable(
        workspace: Option<PathBuf>,
        runtime: RuntimeRoots,
        backend: SandboxBackend,
        reason: SandboxUnavailableReason,
    ) -> Self {
        Self {
            workspace,
            runtime,
            backend,
            launcher: None,
            availability: SandboxAvailability::Unavailable(reason),
            probe: None,
        }
    }

    pub(crate) const fn availability(&self) -> SandboxAvailability {
        self.availability
    }

    pub(crate) const fn backend(&self) -> SandboxBackend {
        self.backend
    }

    /// The availability with its cause attached, using what this sandbox's own
    /// probe observed rather than re-deriving it from the reason alone.
    pub(crate) fn status(&self) -> SandboxStatus {
        let SandboxAvailability::Unavailable(reason) = self.availability else {
            return SandboxStatus::Enforced(self.backend);
        };
        SandboxStatus::Unavailable {
            reason,
            // A probe that never finished taught us nothing, so naming the
            // mounts or the namespace would be a guess.
            cause: match (self.backend, self.probe) {
                (_, Some(ProbeOutcome::TimedOut)) => SandboxFailureCause::ProbeTimedOut,
                (SandboxBackend::Seatbelt, _) => match reason {
                    SandboxUnavailableReason::SeatbeltMissing => {
                        SandboxFailureCause::SeatbeltMissing
                    }
                    SandboxUnavailableReason::GitMetadataSymlink => {
                        SandboxFailureCause::GitMetadataSymlink
                    }
                    SandboxUnavailableReason::InvalidWorkspace => {
                        SandboxFailureCause::InvalidWorkspace
                    }
                    SandboxUnavailableReason::UnsupportedPlatform => {
                        SandboxFailureCause::UnsupportedPlatform
                    }
                    SandboxUnavailableReason::BubblewrapMissing => {
                        SandboxFailureCause::BubblewrapMissing
                    }
                    SandboxUnavailableReason::CannotEnforce => match probe_sandbox_backend() {
                        SandboxStatus::Unavailable { cause, .. } => cause,
                        _ => SandboxFailureCause::SeatbeltProfileRejected,
                    },
                },
                _ => SandboxFailureCause::for_reason(reason),
            },
        }
    }

    /// One line per toolchain config that still holds a registry token the
    /// sandbox cannot mask. Empty is the ordinary case.
    pub(crate) fn advisories(&self) -> Vec<String> {
        self.runtime
            .config_files_holding_a_registry_token()
            .into_iter()
            .map(|config| {
                format!(
                    "note: {} declares a registry token, and agent commands can read it inside \
the sandbox. Euler masks `credentials.toml` but not `config.toml`, which also carries the \
registry and build settings a build needs. Move the token to `credentials.toml` to hide it.",
                    config.display()
                )
            })
            .collect()
    }

    /// Wrap one program invocation in the enforced profile. The caller owns
    /// stdio and timeout configuration on the returned command. An
    /// unavailable profile returns its concise public reason and never gives
    /// the caller an unsandboxed command.
    ///
    /// `env` is the single seam through which a caller adds variables to the
    /// otherwise cleared sandbox environment. Environment *policy* (an
    /// inheritance model and a hard denylist) is a later PR; today the profile
    /// clears everything and sets only what the profile itself needs.
    pub(crate) fn command<I, S>(
        &self,
        program: impl AsRef<OsStr>,
        args: I,
        env: &[(OsString, OsString)],
    ) -> Result<Command, SandboxUnavailableReason>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let SandboxAvailability::Enforced(profile) = self.availability else {
            let SandboxAvailability::Unavailable(reason) = self.availability else {
                unreachable!("availability is either enforced or unavailable");
            };
            return Err(reason);
        };
        let workspace = self
            .workspace
            .as_deref()
            .ok_or(SandboxUnavailableReason::InvalidWorkspace)?;
        let launcher = self
            .launcher
            .as_ref()
            .ok_or(SandboxUnavailableReason::CannotEnforce)?;
        debug_assert_eq!(launcher.backend(), self.backend);
        let launch = SandboxLaunch {
            profile,
            workspace,
            runtime: &self.runtime,
            env,
        };
        Ok(match launcher {
            SandboxLauncher::Bubblewrap(bwrap) => {
                bwrap_command(bwrap, launch, program.as_ref(), args)
            }
            SandboxLauncher::Seatbelt {
                executable,
                _scratch: _,
                scratch_path,
                git_metadata,
                git_metadata_resolved,
            } => seatbelt_command(
                SeatbeltCommand {
                    executable,
                    scratch: scratch_path,
                    git_metadata,
                    git_metadata_resolved,
                },
                launch,
                program.as_ref(),
                args,
            ),
        })
    }
}

/// Resolve both ordinary `.git` directories and Git's regular-file
/// `gitdir:` indirection. Seatbelt matches paths, so protecting only the
/// pointer file would leave an in-workspace metadata target writable.
fn seatbelt_git_metadata_path(git_metadata: &Path) -> Result<PathBuf, SandboxUnavailableReason> {
    let Ok(metadata) = git_metadata.symlink_metadata() else {
        return Ok(git_metadata.to_path_buf());
    };
    if metadata.file_type().is_symlink() {
        return Err(SandboxUnavailableReason::GitMetadataSymlink);
    }
    if metadata.is_dir() {
        return Ok(git_metadata
            .canonicalize()
            .unwrap_or_else(|_| git_metadata.to_path_buf()));
    }
    if !metadata.is_file() || metadata.len() > 16 * 1024 {
        return Err(SandboxUnavailableReason::CannotEnforce);
    }

    let pointer = std::fs::read_to_string(git_metadata)
        .map_err(|_| SandboxUnavailableReason::CannotEnforce)?;
    let pointer = pointer.trim_end_matches(['\r', '\n']);
    if pointer.contains(['\r', '\n']) {
        return Err(SandboxUnavailableReason::CannotEnforce);
    }
    let target = pointer
        .strip_prefix("gitdir: ")
        .filter(|path| !path.is_empty())
        .ok_or(SandboxUnavailableReason::CannotEnforce)?;
    let target = Path::new(target);
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        git_metadata
            .parent()
            .ok_or(SandboxUnavailableReason::CannotEnforce)?
            .join(target)
    };
    let target = target
        .canonicalize()
        .map_err(|_| SandboxUnavailableReason::CannotEnforce)?;
    let workspace = git_metadata
        .parent()
        .ok_or(SandboxUnavailableReason::CannotEnforce)?
        .canonicalize()
        .map_err(|_| SandboxUnavailableReason::CannotEnforce)?;
    if target.starts_with(workspace) {
        return Err(SandboxUnavailableReason::CannotEnforce);
    }
    Ok(target)
}

/// The isolation flags both probes and every launch share. A host that
/// satisfies these in the trivial probe satisfies them in the profile, so the
/// two can never disagree about why the sandbox is unavailable.
const PROFILE_ISOLATION_FLAGS: &[&str] = &[
    "--unshare-user",
    "--unshare-pid",
    "--unshare-ipc",
    "--unshare-uts",
    "--disable-userns",
    "--cap-drop",
    "ALL",
];

const BWRAP_PATHS: &[&str] = &["/usr/bin/bwrap", "/bin/bwrap"];
const SEATBELT_PATH: &str = "/usr/bin/sandbox-exec";
const SEATBELT_PROFILE: &str = include_str!("seatbelt_profile.sbpl");
const SANDBOX_WORKSPACE: &str = "/workspace";
const SANDBOX_HOME: &str = "/tmp/home";
const SANDBOX_CACHE: &str = "/tmp/cache";
/// The host system runtime, read-only.
///
/// `/etc` is not optional on Debian and Ubuntu: every update-alternatives
/// command in `/usr/bin` (`cc`, `c++`, `awk`, `editor`, `java`) is a symlink
/// into `/etc/alternatives`, and `getpwuid` needs `/etc/passwd`, so without it
/// linking fails with "linker `cc` not found" and the child has no user name.
/// Unix permissions still apply inside the user namespace, so this exposes
/// only what the user's own login can already read; `/home` stays invisible
/// (ADR 0014).
const RUNTIME_MOUNTS: &[&str] = &["/usr", "/bin", "/lib", "/lib64", "/etc", "/opt"];
const SYSTEM_SANDBOX_PATH: &str = "/usr/local/bin:/usr/local/sbin:/usr/bin:/usr/sbin:/bin";
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const SANDBOX_READY_MARKER: &str = "__EULER_SANDBOX_READY__\n";
const SANDBOX_READY_WRAPPER: &str = "printf '__EULER_SANDBOX_READY__\\n'; exec \"$@\"";
/// A toolchain root must be a real subtree, never `/`, a host home, or a
/// single-component directory whose contents are unrelated to a toolchain.
const MIN_RUNTIME_ROOT_COMPONENTS: usize = 2;
/// Paths the profile itself mounts. Nothing else may be remounted over them,
/// which is what `read_only_parents` uses this for.
const PROFILE_MOUNT_POINTS: &[&str] = &[
    "/tmp",
    "/proc",
    "/dev",
    SANDBOX_WORKSPACE,
    SANDBOX_HOME,
    SANDBOX_CACHE,
];

/// The subset a toolchain root may not be, contain, or sit inside, at
/// detection time.
///
/// `/tmp` is absent on purpose — see [`names_a_profile_mount_point`] — and
/// reached anyway through the sandbox home beneath it. The sandbox workspace
/// is absent because whether it collides depends on where the host workspace
/// is: a container that mounts the project at `/workspace` has a real
/// `CARGO_HOME=/workspace/.cargo`, which is re-pointed rather than mounted.
/// [`RuntimeRoots::excluding`] decides that once it knows the workspace.
const EXCLUSIVE_PROFILE_MOUNTS: &[&str] = &["/proc", "/dev", SANDBOX_HOME, SANDBOX_CACHE];
#[cfg(target_os = "linux")]
const FIRST_INHERITED_FD: libc::c_uint = 3;
#[cfg(target_os = "linux")]
const CLOSE_RANGE_CLOEXEC: libc::c_ulong = 1 << 2;
#[cfg(target_os = "linux")]
const PROC_FD_DIRECTORY: &[u8] = b"/proc/self/fd\0";
#[cfg(target_os = "linux")]
const PROC_DIRENT64_RECLEN_OFFSET: usize = 16;
#[cfg(target_os = "linux")]
const PROC_DIRENT64_NAME_OFFSET: usize = 19;
#[cfg(target_os = "linux")]
const PROC_FD_BUFFER_LEN: usize = 4096;

/// Toolchain homes the host environment implies, read-only inside the
/// sandbox at their real paths (ADR 0021 row A′).
///
/// `HOME` inside the sandbox is a private tmpfs, so a toolchain installed
/// under the real home is otherwise unreachable and `cargo build` fails with
/// command-not-found. Detection is by environment variable first and
/// conventional location second; the real home itself is never mounted, and
/// the directory that holds these roots is remounted read-only so a write
/// under the real home fails rather than landing in a discarded private copy.
///
/// The one exception is a holding directory that is also one of the profile's
/// own mount points (`HOME=/tmp`): remounting it would shadow the sandbox
/// home, cache and TMPDIR, so it is left writable and a write there lands in
/// the private tmpfs instead of failing. Nothing of the host is exposed
/// either way.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RuntimeRoots {
    /// Read-only mount sources, canonical and non-overlapping.
    roots: Vec<PathBuf>,
    /// `NAME=value` pairs pointing at those roots, so a toolchain finds
    /// itself at the same path it occupies on the host.
    variables: Vec<(OsString, OsString)>,
    /// `PATH` entries from the host that live inside a mounted root.
    path_entries: Vec<PathBuf>,
    /// The real home directory, when it exists and is not a mounted root.
    home: Option<PathBuf>,
    /// The writable workspace, once known. A toolchain home inside it needs
    /// no mount of its own, but its variable must point at the bound path.
    workspace: Option<PathBuf>,
}

/// Environment variables that name a toolchain home, paired with the
/// conventional location used when the variable is unset.
const TOOLCHAIN_ROOTS: &[(&str, &str)] = &[
    ("CARGO_HOME", ".cargo"),
    ("RUSTUP_HOME", ".rustup"),
    ("NVM_DIR", ".nvm"),
    ("PYENV_ROOT", ".pyenv"),
    ("ASDF_DATA_DIR", ".asdf"),
    ("GOPATH", "go"),
    ("PNPM_HOME", ".local/share/pnpm"),
];

/// Toolchain stores that are not under a home directory and carry no
/// environment variable of their own.
const SYSTEM_TOOLCHAIN_ROOTS: &[&str] = &["/nix/store"];

/// Credential files inside a Cargo home. Mounting `CARGO_HOME` read-only
/// makes the registry token readable, where main returned ENOENT for it.
/// `config.toml` stays visible because a build needs it.
///
/// These names are Cargo's, so they are applied only to the Cargo home. A
/// file called `credentials` under an unrelated toolchain root belongs to
/// something else and is not Euler's to hide.
const MASKED_CARGO_FILES: &[&str] = &["credentials.toml", "credentials"];

/// The Cargo configuration files, in the order Cargo itself prefers. Both are
/// read, and both can carry a registry token.
const CARGO_CONFIG_FILES: &[&str] = &["config.toml", "config"];

impl RuntimeRoots {
    /// Read the host environment. This never consults the workspace: an agent
    /// must not be able to add a mount by writing a file.
    pub(crate) fn detect() -> Self {
        Self::from_environment(
            std::env::var_os("HOME").map(PathBuf::from),
            |name| std::env::var_os(name),
            std::env::var_os("PATH"),
        )
    }

    fn from_environment(
        home: Option<PathBuf>,
        variable: impl Fn(&str) -> Option<OsString>,
        path: Option<OsString>,
    ) -> Self {
        let home = home.and_then(|home| home.canonicalize().ok());
        let mut roots = Vec::new();
        let mut variables = Vec::new();
        for (name, conventional) in TOOLCHAIN_ROOTS {
            let candidate = variable(name)
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(conventional)));
            // A home-relative default is a guess, so it must be a real
            // subtree; a value the user set explicitly is a statement, and
            // the official Go images set `GOPATH=/go`.
            let explicit = variable(name).is_some();
            let Some(root) =
                candidate.and_then(|root| usable_runtime_root(&root, home.as_deref(), explicit))
            else {
                continue;
            };
            variables.push((OsString::from(*name), root.clone().into_os_string()));
            // A toolchain inside the system runtime is already reachable
            // (the Rust images put CARGO_HOME at /usr/local/cargo). Mounting
            // it again would shadow the bind that already carries it.
            if !RUNTIME_MOUNTS.iter().any(|mount| root.starts_with(mount)) {
                roots.push(root);
            }
        }
        for root in SYSTEM_TOOLCHAIN_ROOTS {
            if let Some(root) = usable_runtime_root(Path::new(root), home.as_deref(), false) {
                if !RUNTIME_MOUNTS.iter().any(|mount| root.starts_with(mount)) {
                    roots.push(root);
                }
            }
        }
        let mut runtime = Self {
            roots,
            variables,
            path_entries: Vec::new(),
            home,
            workspace: None,
        };
        runtime.normalize();
        runtime.path_entries = runtime.host_path_entries_inside_roots(path.as_deref());
        runtime
    }

    /// Drop overlapping and duplicate *mounts*, keeping the outermost of any
    /// nested pair so Bubblewrap never receives two binds for one subtree.
    ///
    /// Variables are never dropped with them. `RUSTUP_HOME=$CARGO_HOME/rustup`
    /// needs only one mount but both variables; losing `RUSTUP_HOME` breaks
    /// every rustup proxy.
    fn normalize(&mut self) {
        self.roots.sort();
        self.roots.dedup();
        let mut kept: Vec<PathBuf> = Vec::new();
        for root in std::mem::take(&mut self.roots) {
            if kept.iter().any(|existing| root.starts_with(existing)) {
                continue;
            }
            kept.push(root);
        }
        self.roots = kept;
    }

    /// The Cargo home the sandbox can reach, whether Euler mounts it or the
    /// system runtime already carries it.
    ///
    /// A Cargo home inside the workspace is excluded: the workspace is mounted
    /// read-write and readable by design, so nothing there was newly exposed
    /// by a toolchain mount and masking it would only hide the user's own
    /// project files from them.
    fn cargo_home(&self) -> Option<PathBuf> {
        let value = self
            .variables
            .iter()
            .find(|(name, _)| name == "CARGO_HOME")
            .map(|(_, value)| PathBuf::from(value))?;
        (self.reachable(&value) && !self.inside_workspace(&value)).then_some(value)
    }

    /// Cargo also accepts a registry token in its config, which Euler does not
    /// mask: that file carries registry sources and build settings, so masking
    /// it would break the build. Report it instead, so the user can move the
    /// token to `credentials.toml`, which is masked.
    pub(crate) fn config_files_holding_a_registry_token(&self) -> Vec<PathBuf> {
        let Some(home) = self.cargo_home() else {
            return Vec::new();
        };
        CARGO_CONFIG_FILES
            .iter()
            .map(|name| home.join(name))
            .filter(|config| file_declares_a_token(config))
            .collect()
    }

    /// Whether a path is still usable once the workspace is known: reachable
    /// through a mount, or inside the workspace and therefore re-pointed —
    /// and in neither case shadowed by the workspace bind.
    fn usable_after_workspace(&self, path: &Path) -> bool {
        let Some(workspace) = self.workspace.as_deref() else {
            return self.reachable(path);
        };
        (self.reachable(path) || self.inside_workspace(path))
            && !shadowed_by_the_workspace_bind(path, workspace)
    }

    fn inside_workspace(&self, path: &Path) -> bool {
        self.workspace
            .as_ref()
            .is_some_and(|workspace| path.starts_with(workspace))
    }

    /// The value a path takes inside the sandbox. The workspace is bound at
    /// [`SANDBOX_WORKSPACE`], so a toolchain home the user keeps inside their
    /// project is reachable there rather than at its host path.
    fn sandbox_view(&self, host: &Path) -> PathBuf {
        self.workspace
            .as_ref()
            .and_then(|workspace| host.strip_prefix(workspace).ok())
            .map_or_else(
                || host.to_path_buf(),
                |relative| Path::new(SANDBOX_WORKSPACE).join(relative),
            )
    }

    /// Every path the sandbox can reach read-only: the roots it mounts plus
    /// the system runtime it already binds. A toolchain inside the system
    /// runtime (`CARGO_HOME=/usr/local/cargo` in the official Rust images)
    /// needs no mount of its own but still needs its variable and its `PATH`.
    fn reachable(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| path.starts_with(root))
            || RUNTIME_MOUNTS.iter().any(|mount| path.starts_with(mount))
    }

    /// Keep only the host `PATH` entries that a mounted root actually
    /// provides. Version-manager layouts (`~/.nvm/versions/node/*/bin`,
    /// `~/.pyenv/shims`) are not derivable from the root alone, so the host
    /// `PATH` is the evidence for what is reachable — filtered by containment
    /// so nothing outside a mounted root enters the sandbox `PATH`.
    fn host_path_entries_inside_roots(&self, path: Option<&OsStr>) -> Vec<PathBuf> {
        let Some(path) = path else {
            return Vec::new();
        };
        let mut seen = BTreeSet::new();
        std::env::split_paths(path)
            .filter(|entry| entry.is_absolute())
            // Canonical form is what the sandbox mounts, so containment is
            // decided against it: a symlinked `PATH` entry must not smuggle
            // in a directory no root covers, nor be dropped for spelling.
            .filter_map(|entry| entry.canonicalize().ok())
            .filter(|entry| self.reachable(entry))
            .filter(|entry| seen.insert(entry.clone()))
            .collect()
    }

    /// Remove any root that overlaps the writable workspace: the workspace is
    /// mounted read-write and must not also appear read-only.
    ///
    /// The variables and `PATH` entries of a dropped root survive only when
    /// something else still makes the path reachable.
    fn excluding(mut self, workspace: &Path) -> Self {
        self.roots
            .retain(|root| !root.starts_with(workspace) && !workspace.starts_with(root));
        self.roots
            .retain(|root| !shadowed_by_the_workspace_bind(root, workspace));
        self.normalize();
        self.workspace = Some(workspace.to_path_buf());
        // A toolchain home inside the workspace keeps its variable and its
        // `PATH`: the directory really is there, bound read-write, so
        // `CARGO_HOME=<workspace>/.cargo` would otherwise be unset inside the
        // sandbox with the toolchain sitting in plain sight. `sandbox_view`
        // rewrites such a value to its bound path.
        let mut variables = std::mem::take(&mut self.variables);
        variables.retain(|(_, value)| self.usable_after_workspace(Path::new(value)));
        self.variables = variables;
        let mut path_entries = std::mem::take(&mut self.path_entries);
        path_entries.retain(|entry| self.usable_after_workspace(entry));
        self.path_entries = path_entries;
        self
    }

    /// Directories that must become read-only mount points so that a write
    /// anywhere under the real home fails instead of silently landing in the
    /// sandbox's private tmpfs.
    fn read_only_parents(&self) -> Vec<PathBuf> {
        let mut parents = self
            .roots
            .iter()
            .filter_map(|root| root.parent())
            .filter(|parent| parent.components().count() >= MIN_RUNTIME_ROOT_COMPONENTS)
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        if let Some(home) = &self.home {
            if home.components().count() >= MIN_RUNTIME_ROOT_COMPONENTS {
                parents.push(home.clone());
            }
        }
        parents.sort();
        parents.dedup();
        // A holding directory that is also one of the profile's own mount
        // points must not be re-mounted: `CARGO_HOME=/tmp/cargo` would
        // otherwise put a second tmpfs over `/tmp` after the sandbox home and
        // cache were created there, and then remount it read-only.
        parents.retain(|parent| {
            !RUNTIME_MOUNTS.iter().any(|mount| parent.starts_with(mount))
                && !PROFILE_MOUNT_POINTS
                    .iter()
                    .any(|mount| parent.starts_with(mount) || Path::new(mount).starts_with(parent))
        });
        parents
    }

    /// The `PATH` the sandbox exports: mounted toolchain directories first,
    /// then the system runtime.
    fn sandbox_path(&self) -> OsString {
        let mut entries = self
            .path_entries
            .iter()
            .map(|entry| self.sandbox_view(entry))
            .collect::<Vec<_>>();
        entries.push(PathBuf::from(SYSTEM_SANDBOX_PATH));
        entries
            .iter()
            .map(|entry| entry.as_os_str().to_os_string())
            .collect::<Vec<_>>()
            .join(OsStr::new(":"))
    }

    /// The same detected toolchain path in the host filesystem view used by
    /// Seatbelt, which filters access without constructing a new root.
    fn seatbelt_path(&self) -> OsString {
        let mut entries = self.path_entries.clone();
        entries.extend(std::env::split_paths(OsStr::new(SYSTEM_SANDBOX_PATH)));
        entries
            .iter()
            .map(|entry| entry.as_os_str().to_os_string())
            .collect::<Vec<_>>()
            .join(OsStr::new(":"))
    }
}

/// Accept a toolchain root only when it is an existing directory that is not
/// the home itself.
///
/// A depth of at least two guards a *guessed* root: `$HOME/go` is a guess, and
/// a one-component guess would be a whole top-level directory. An `explicit`
/// root came from a toolchain variable the user or image set, so a
/// two-component path like the Go images' `GOPATH=/go` is an answer, not an
/// accident. Neither form may be the home, and callers still exclude anything
/// already carried by a system runtime mount.
fn usable_runtime_root(root: &Path, home: Option<&Path>, explicit: bool) -> Option<PathBuf> {
    // Checked before and after canonicalization: `/tmp` is a symlink on some
    // hosts, and either spelling names the same directory.
    if names_a_profile_mount_point(root) {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let depth = root.components().count();
    if !root.is_dir() || depth <= 1 || (!explicit && depth <= MIN_RUNTIME_ROOT_COMPONENTS) {
        return None;
    }
    if home.is_some_and(|home| home == root || home.starts_with(&root)) {
        return None;
    }
    if names_a_profile_mount_point(&root) {
        return None;
    }
    Some(root)
}

/// Whether the workspace bind would cover this path without it being the
/// workspace's own content.
///
/// A path inside the host workspace is re-pointed to [`SANDBOX_WORKSPACE`] and
/// really is there, so it is not shadowed. Anything else that occupies the
/// sandbox workspace path would simply disappear under the bind.
fn shadowed_by_the_workspace_bind(path: &Path, workspace: &Path) -> bool {
    if path.starts_with(workspace) {
        return false;
    }
    path.starts_with(SANDBOX_WORKSPACE) || Path::new(SANDBOX_WORKSPACE).starts_with(path)
}

/// Whether a path would collide with one of the fixed mounts in
/// [`EXCLUSIVE_PROFILE_MOUNTS`]: `/proc`, `/dev`, and the sandbox home and
/// cache. A path that is one of them, contains one, or sits inside one
/// collides; the sandbox workspace is decided separately, by
/// [`shadowed_by_the_workspace_bind`], once the host workspace is known.
///
/// `/tmp` is not in the set and is caught only indirectly, by being an
/// ancestor of the sandbox home beneath it — which is the whole rule. A root
/// merely *inside* `/tmp` is deliberately allowed: the profile lays down its
/// tmpfs first, so the bind lands on the fresh directory and shadows nothing.
/// `GOPATH=/tmp` itself is rejected, because a read-only bind there would
/// replace the private `/tmp` the sandbox home, cache and TMPDIR live in, and
/// the resulting probe failure would be reported as something about /usr.
fn names_a_profile_mount_point(path: &Path) -> bool {
    EXCLUSIVE_PROFILE_MOUNTS
        .iter()
        .any(|mount| path.starts_with(mount) || Path::new(mount).starts_with(path))
}

/// Probe whether the default profile is actually enforceable for `workspace`.
///
/// The child process gets a private root, can access its workspace, cannot
/// write under the real home, and must enter a network namespace. A failure is
/// intentionally collapsed to a stable public reason: raw Bubblewrap
/// diagnostics may expose host details and are not suitable for model-facing
/// or transcript output.
pub fn probe_workspace_sandbox(workspace: &Path) -> SandboxStatus {
    WorkspaceSandbox::new(workspace, SandboxProfile::WorkspaceNoNetwork).status()
}

/// Probe the execution boundary itself, independent of any workspace.
///
/// Bubblewrap being installed is not evidence that it works: Ubuntu 23.10+
/// AppArmor, hardened sysctls, most containers, and WSL1 all leave a working
/// binary that cannot create a user namespace. The only reliable test is to
/// run a trivial sandboxed command, so that is what this does.
///
/// Bundled `bwrap`: Codex ships a SHA256-verified `bwrap` binary. Euler
/// requires a system `bwrap` for now — vendoring the C source would add a C
/// toolchain to `cargo build`, and fetching a pinned release asset would add a
/// network dependency to it. Both are disproportionate while this diagnostic
/// tells the user exactly what to install; bundling belongs to the release
/// workflow instead, tracked as issue #230.
pub fn probe_sandbox_backend() -> SandboxStatus {
    // The backend is a property of the host, not of a workspace, and cannot
    // change under a running process: probe it once. The per-workspace profile
    // probe still runs for every registry.
    static BACKEND: OnceLock<SandboxStatus> = OnceLock::new();
    *BACKEND.get_or_init(probe_sandbox_backend_uncached)
}

fn probe_sandbox_backend_uncached() -> SandboxStatus {
    if cfg!(target_os = "macos") {
        let Some(seatbelt) = seatbelt_path() else {
            return SandboxStatus::Unavailable {
                reason: SandboxUnavailableReason::SeatbeltMissing,
                cause: SandboxFailureCause::SeatbeltMissing,
            };
        };
        let mut command = Command::new(seatbelt);
        command
            .env_clear()
            .args([
                "-p",
                "(version 1) (deny default) (allow process-exec) (allow file-read*)",
                "--",
            ])
            .arg("/usr/bin/true");
        mark_inherited_fds_close_on_exec(&mut command);
        let cause = match run_probe_to_completion(command) {
            ProbeOutcome::Succeeded => return SandboxStatus::Enforced(SandboxBackend::Seatbelt),
            ProbeOutcome::TimedOut => SandboxFailureCause::ProbeTimedOut,
            ProbeOutcome::Refused => SandboxFailureCause::SeatbeltProfileRejected,
        };
        return SandboxStatus::Unavailable {
            reason: SandboxUnavailableReason::CannotEnforce,
            cause,
        };
    }
    if !cfg!(target_os = "linux") {
        return SandboxStatus::Host;
    }
    let Some(bwrap) = bwrap_path() else {
        return SandboxStatus::Unavailable {
            reason: SandboxUnavailableReason::BubblewrapMissing,
            cause: SandboxFailureCause::BubblewrapMissing,
        };
    };
    let mut command = Command::new(&bwrap);
    command.env_clear();
    mark_inherited_fds_close_on_exec(&mut command);
    // The same isolation flags the profile uses, so a host that passes here
    // cannot fail the profile probe and be told to fix its userns sysctls.
    // `--disable-userns` needs Bubblewrap 0.8.0, which Ubuntu 22.04 and
    // Debian 11 predate; that is its own cause, not a userns problem.
    command.args(PROFILE_ISOLATION_FLAGS);
    command.args(["--unshare-net", "--ro-bind", "/", "/", "/bin/true"]);
    let cause = match run_probe_to_completion(command) {
        ProbeOutcome::Succeeded => return SandboxStatus::Enforced(SandboxBackend::Bwrap),
        ProbeOutcome::TimedOut => SandboxFailureCause::ProbeTimedOut,
        ProbeOutcome::Refused => attribute_isolation_failure(&bwrap),
    };
    SandboxStatus::Unavailable {
        reason: SandboxUnavailableReason::CannotEnforce,
        cause,
    }
}

/// Decide why the isolation flags failed, by trying the same probe without
/// the newest one.
///
/// `--disable-userns` needs Bubblewrap 0.8.0. If dropping it makes an
/// otherwise identical probe succeed, the flag is what this build lacks;
/// if the probe still fails, the namespace itself is the problem. Asking
/// `bwrap --help` instead would depend on which stream a build prints to
/// and on flag names appearing verbatim in prose.
///
/// Only reached after a refusal, never after a timeout: a retry that happened
/// to win a race would otherwise tell a healthy host to upgrade Bubblewrap.
fn attribute_isolation_failure(bwrap: &Path) -> SandboxFailureCause {
    let mut command = Command::new(bwrap);
    command.env_clear();
    mark_inherited_fds_close_on_exec(&mut command);
    command.args(
        PROFILE_ISOLATION_FLAGS
            .iter()
            .filter(|flag| **flag != "--disable-userns"),
    );
    command.args(["--unshare-net", "--ro-bind", "/", "/", "/bin/true"]);
    match run_probe_to_completion(command) {
        ProbeOutcome::Succeeded => SandboxFailureCause::BubblewrapTooOld,
        // The retry taught us nothing either, so naming the namespace would
        // be the same guess the outcome split exists to prevent.
        ProbeOutcome::TimedOut => SandboxFailureCause::ProbeTimedOut,
        ProbeOutcome::Refused => user_namespace_failure_cause(),
    }
}

/// Attribute a user-namespace failure to something the user can verify and
/// change. Every branch reads a host fact rather than parsing Bubblewrap's
/// stderr, which is unstable across versions and host-revealing.
fn user_namespace_failure_cause() -> SandboxFailureCause {
    if read_trimmed("/proc/sys/kernel/osrelease")
        .is_some_and(|release| release.contains("Microsoft") && !release.contains("WSL2"))
    {
        return SandboxFailureCause::Wsl1;
    }
    if read_trimmed("/proc/sys/kernel/unprivileged_userns_clone").as_deref() == Some("0")
        || read_trimmed("/proc/sys/user/max_user_namespaces").as_deref() == Some("0")
    {
        return SandboxFailureCause::UserNamespacesDisabled;
    }
    if read_trimmed("/proc/sys/kernel/apparmor_restrict_unprivileged_userns").as_deref()
        == Some("1")
    {
        return SandboxFailureCause::AppArmorUserNamespaceRestriction;
    }
    if Path::new("/.dockerenv").exists()
        || read_trimmed("/proc/1/cgroup")
            .is_some_and(|cgroup| cgroup.contains("docker") || cgroup.contains("lxc"))
    {
        return SandboxFailureCause::Container;
    }
    SandboxFailureCause::Unattributed
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
}

/// Remove the private prelude emitted only after Bubblewrap has completed its
/// setup and entered the inner command. If it is absent, the launcher failed
/// before the child started, so callers must not surface its raw diagnostics.
pub(crate) fn strip_sandbox_ready_marker(stdout: &str) -> Result<&str, SandboxUnavailableReason> {
    stdout
        .strip_prefix(SANDBOX_READY_MARKER)
        .ok_or(SandboxUnavailableReason::CannotEnforce)
}

fn probe_bwrap_profile(
    bwrap: &Path,
    workspace: &Path,
    runtime: &RuntimeRoots,
    profile: SandboxProfile,
) -> (SandboxAvailability, ProbeOutcome) {
    // `test -d` for the workspace, not `test -w`: the mount is what the probe
    // is checking, and a read-only checkout is a workspace Euler can still
    // read. `/tmp` must be writable, because the sandbox home, cache and
    // TMPDIR all live there and a stray mount over it would be silent.
    let mut script = String::from("test -d /workspace && test -d /usr && test -w /tmp");
    // Assert the real home is read-only only where the profile actually
    // remounted it. A one-component home is never a mount point of its own,
    // and a home that is itself a profile mount point (`HOME=/tmp`) is
    // deliberately left writable, so asserting either would fail a working
    // sandbox and send the user to the user-namespace diagnostic.
    let read_only_parents = runtime.read_only_parents();
    if let Some(home) = runtime
        .home
        .as_ref()
        .filter(|home| read_only_parents.contains(home))
    {
        script.push_str(&format!(" && test ! -w {}", shell_quote(home)));
    }
    let mut command = bwrap_command(
        bwrap,
        SandboxLaunch {
            profile,
            workspace,
            runtime,
            env: &[],
        },
        OsStr::new("/bin/sh"),
        ["-c", script.as_str()],
    );
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let outcome = run_probe_to_completion(command);
    let availability = if outcome.succeeded() {
        SandboxAvailability::Enforced(profile)
    } else {
        SandboxAvailability::Unavailable(SandboxUnavailableReason::CannotEnforce)
    };
    (availability, outcome)
}

fn probe_seatbelt_profile(
    launcher: &SandboxLauncher,
    workspace: &Path,
    runtime: &RuntimeRoots,
    profile: SandboxProfile,
) -> ProbeOutcome {
    let SandboxLauncher::Seatbelt {
        executable,
        _scratch: _,
        scratch_path,
        git_metadata,
        git_metadata_resolved,
    } = launcher
    else {
        unreachable!("Seatbelt profile probe requires a Seatbelt launcher");
    };
    let command = seatbelt_command(
        SeatbeltCommand {
            executable,
            scratch: scratch_path,
            git_metadata,
            git_metadata_resolved,
        },
        SandboxLaunch {
            profile,
            workspace,
            runtime,
            env: &[],
        },
        OsStr::new("/bin/sh"),
        [
            "-c",
            "probe=\"$TMPDIR/euler-seatbelt-probe-$$\"; printf ready >\"$probe\" && rm -f \"$probe\"",
        ],
    );
    run_probe_to_completion(command)
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// Run one bounded probe. A probe that outlives its deadline is killed and
/// reported as [`ProbeOutcome::TimedOut`], distinct from a refusal: the
/// sandbox must never make session start hang, and a deadline that expired
/// says nothing about why.
fn run_probe_to_completion(mut command: Command) -> ProbeOutcome {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = command.spawn() else {
        return ProbeOutcome::Refused;
    };
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return ProbeOutcome::Succeeded,
            Ok(Some(_)) | Err(_) => return ProbeOutcome::Refused,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return ProbeOutcome::TimedOut;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// How a probe ended.
///
/// `Refused` and `TimedOut` are deliberately separate. A refusal is evidence —
/// Bubblewrap looked at the request and said no — while a timeout is the
/// absence of evidence, and attributing a cause to it would send a user on a
/// loaded machine to fix something that is not broken.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProbeOutcome {
    Succeeded,
    Refused,
    TimedOut,
}

impl ProbeOutcome {
    const fn succeeded(self) -> bool {
        matches!(self, Self::Succeeded)
    }
}

/// Do not discover Bubblewrap through the caller's `PATH`: an agent workspace
/// or inherited shell configuration must not substitute the sandbox launcher.
fn bwrap_path() -> Option<PathBuf> {
    BWRAP_PATHS
        .iter()
        .map(Path::new)
        .find(|path| path.is_file())
        .map(Path::to_path_buf)
}

/// Seatbelt is part of macOS, and the trusted system path prevents a
/// workspace-controlled `PATH` from substituting the policy launcher.
fn seatbelt_path() -> Option<PathBuf> {
    let path = Path::new(SEATBELT_PATH);
    path.is_file().then(|| path.to_path_buf())
}

fn seatbelt_scratch() -> Option<(tempfile::TempDir, PathBuf)> {
    let scratch = tempfile::Builder::new()
        .prefix("euler-seatbelt-")
        .tempdir()
        .ok()?;
    let scratch_path = scratch.path().canonicalize().ok()?;
    for directory in ["home", "cache", "tmp"] {
        std::fs::create_dir(scratch_path.join(directory)).ok()?;
    }
    Some((scratch, scratch_path))
}

fn canonical_workspace(workspace: &Path) -> Result<PathBuf, std::io::Error> {
    let workspace = workspace.canonicalize()?;
    if workspace.is_dir() {
        Ok(workspace)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "workspace root is not a directory",
        ))
    }
}

/// Everything the profile mounts and exports for one launch.
struct SandboxLaunch<'a> {
    profile: SandboxProfile,
    workspace: &'a Path,
    runtime: &'a RuntimeRoots,
    env: &'a [(OsString, OsString)],
}

#[derive(Clone, Copy)]
struct SeatbeltCommand<'a> {
    executable: &'a Path,
    scratch: &'a Path,
    git_metadata: &'a Path,
    git_metadata_resolved: &'a Path,
}

fn bwrap_command<I, S>(bwrap: &Path, launch: SandboxLaunch<'_>, program: &OsStr, args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let SandboxLaunch {
        profile,
        workspace,
        runtime,
        env,
    } = launch;
    let mut command = Command::new(bwrap);
    // Clear the launcher too: `--clearenv` protects the inner command, while
    // this prevents an inherited loader/configuration variable from changing
    // Bubblewrap before it establishes the namespace.
    command.env_clear();
    mark_inherited_fds_close_on_exec(&mut command);
    command.args(PROFILE_ISOLATION_FLAGS);
    command.args(["--die-with-parent", "--new-session", "--clearenv"]);
    for (name, value) in sandbox_environment(runtime, env) {
        command.arg("--setenv").arg(name).arg(value);
    }
    command.args(["--tmpfs", "/"]);
    match profile {
        SandboxProfile::WorkspaceNoNetwork => command.arg("--unshare-net"),
    };
    for mount in RUNTIME_MOUNTS {
        if Path::new(mount).exists() {
            command.args(["--dir", mount, "--ro-bind", mount, mount]);
        }
    }
    command.args([
        "--dir",
        "/proc",
        "--proc",
        "/proc",
        "--dir",
        "/dev",
        "--dev",
        "/dev",
        "--dir",
        "/tmp",
        "--tmpfs",
        "/tmp",
        "--dir",
        SANDBOX_HOME,
        "--dir",
        SANDBOX_CACHE,
    ]);
    add_runtime_root_mounts(&mut command, runtime);
    command
        .args(["--dir", SANDBOX_WORKSPACE, "--bind"])
        .arg(workspace)
        .arg(SANDBOX_WORKSPACE)
        .args(["--chdir", SANDBOX_WORKSPACE, "--", "/bin/sh", "-c"])
        .arg(SANDBOX_READY_WRAPPER)
        .arg("euler-sandbox")
        .arg(program)
        .args(args);
    command
}

fn seatbelt_command<I, S>(
    seatbelt: SeatbeltCommand<'_>,
    launch: SandboxLaunch<'_>,
    program: &OsStr,
    args: I,
) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let SeatbeltCommand {
        executable,
        scratch,
        git_metadata,
        git_metadata_resolved,
    } = seatbelt;
    let SandboxLaunch {
        profile,
        workspace,
        runtime,
        env,
    } = launch;
    match profile {
        SandboxProfile::WorkspaceNoNetwork => {}
    }
    let mut command = Command::new(executable);
    command
        .env_clear()
        .current_dir(workspace)
        .arg("-p")
        .arg(SEATBELT_PROFILE)
        .arg(seatbelt_definition("WORKSPACE", workspace))
        .arg(seatbelt_definition("GIT_METADATA", git_metadata))
        .arg(seatbelt_definition(
            "GIT_METADATA_RESOLVED",
            git_metadata_resolved,
        ))
        .arg(seatbelt_definition("SCRATCH", scratch))
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(SANDBOX_READY_WRAPPER)
        .arg("euler-sandbox")
        .arg(program)
        .args(args);
    for (name, value) in seatbelt_environment(runtime, scratch, env) {
        command.env(name, value);
    }
    mark_inherited_fds_close_on_exec(&mut command);
    command
}

fn seatbelt_definition(name: &str, path: &Path) -> OsString {
    let mut definition = OsString::from("-D");
    definition.push(name);
    definition.push("=");
    definition.push(path);
    definition
}

/// Mount the toolchain roots read-only at their real paths.
///
/// Each holding directory becomes a tmpfs mount point first and is remounted
/// read-only last. Without the remount the holding directory would be an
/// ordinary writable directory in the sandbox's private root tmpfs, so a write
/// under the real home would appear to succeed and be silently discarded.
fn add_runtime_root_mounts(command: &mut Command, runtime: &RuntimeRoots) {
    let parents = runtime.read_only_parents();
    for parent in &parents {
        command.arg("--tmpfs").arg(parent);
    }
    for root in &runtime.roots {
        command.arg("--ro-bind").arg(root).arg(root);
    }
    // The Cargo home the sandbox can reach, not only one Euler mounts itself:
    // `CARGO_HOME=/usr/local/cargo` in the official Rust images is carried by
    // the wholesale `/usr` bind, so masking only mounted roots would leave its
    // token readable in exactly the common containerized case.
    if let Some(home) = runtime.cargo_home() {
        mask_cargo_credentials(command, &home);
    }
    for parent in &parents {
        command.arg("--remount-ro").arg(parent);
    }
}

/// Whether a Cargo config declares a token of any kind.
///
/// Deliberately over-broad, and deliberately not a TOML parse. A token can be
/// written as `token = …` under `[registry]`, as a dotted `registry.token` at
/// top level, inside an inline table, or under a quoted table name — and there
/// is no TOML parser in this workspace to tell them apart. A Cargo config has
/// no other legitimate `token` key, so a false positive costs one extra
/// advisory line while a false negative is a silently exposed credential.
/// Any line whose key is `token` or ends in `.token` counts.
///
/// The value is never read, only the key.
fn file_declares_a_token(config: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(config) else {
        return false;
    };
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .any(line_assigns_a_token_key)
}

/// Whether one line assigns to a key named `token`.
///
/// Scans for the word rather than parsing, so `token =`, `registry.token =`
/// and `registry = { token = … }` all count while `tokenizer =` does not. The
/// word must start a key — preceded by nothing, whitespace, a dot, a brace, a
/// comma or a quote — and be followed by `=`.
fn line_assigns_a_token_key(line: &str) -> bool {
    let mut rest = line;
    while let Some(at) = rest.find("token") {
        let before = rest[..at].chars().next_back();
        let starts_key = before.is_none_or(|character| {
            character.is_whitespace() || matches!(character, '.' | '{' | ',' | '"' | '\'')
        });
        let after = rest[at + "token".len()..].trim_start();
        let after = after
            .strip_prefix(['"', '\''])
            .unwrap_or(after)
            .trim_start();
        if starts_key && after.starts_with('=') {
            return true;
        }
        rest = &rest[at + "token".len()..];
    }
    false
}

/// Cover the credential files inside the reachable Cargo home with an empty
/// file. Read-only is not enough: `main` returned ENOENT for a registry token
/// that the mount would now make readable.
///
/// Only files that exist on the host are masked, because Bubblewrap cannot
/// create a mount point inside a read-only bind.
fn mask_cargo_credentials(command: &mut Command, home: &Path) {
    for name in MASKED_CARGO_FILES {
        let path = home.join(name);
        if path.is_file() {
            command.arg("--ro-bind-try").arg("/dev/null").arg(path);
        }
    }
}

/// The complete sandbox environment, in one place.
///
/// ADR 0021 row C replaces this with an inheritance model and a tiered
/// denylist in a later PR; until then the profile clears the environment and
/// sets exactly what it mounts, so there is one seam to change.
fn sandbox_environment(
    runtime: &RuntimeRoots,
    extra: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    let mut environment = vec![
        (OsString::from("HOME"), OsString::from(SANDBOX_HOME)),
        (
            OsString::from("XDG_CACHE_HOME"),
            OsString::from(SANDBOX_CACHE),
        ),
        (OsString::from("TMPDIR"), OsString::from("/tmp")),
        (OsString::from("PATH"), runtime.sandbox_path()),
    ];
    environment.extend(runtime.variables.iter().map(|(name, value)| {
        (
            name.clone(),
            runtime.sandbox_view(Path::new(value)).into_os_string(),
        )
    }));
    environment.extend(extra.iter().cloned());
    environment
}

fn seatbelt_environment(
    runtime: &RuntimeRoots,
    scratch: &Path,
    extra: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    let mut environment = vec![
        (
            OsString::from("HOME"),
            scratch.join("home").into_os_string(),
        ),
        (
            OsString::from("XDG_CACHE_HOME"),
            scratch.join("cache").into_os_string(),
        ),
        (
            OsString::from("TMPDIR"),
            scratch.join("tmp").into_os_string(),
        ),
        (OsString::from("PATH"), runtime.seatbelt_path()),
    ];
    environment.extend(runtime.variables.iter().cloned());
    environment.extend(extra.iter().cloned());
    environment
}

/// Keep non-stdio host descriptors out of Bubblewrap and the agent command.
/// A readable file or connected socket inherited from Euler would otherwise
/// bypass the mount and network boundary through `/proc/self/fd`.
///
/// `CLOEXEC` preserves Rust's private spawn-error pipe until `exec`, while
/// ensuring Bubblewrap and its inner command receive only standard I/O. Linux
/// 5.11+ can set the bit atomically with `close_range`; older kernels use a
/// post-fork `/proc/self/fd` syscall scan and therefore remain supported.
#[cfg(target_os = "linux")]
fn mark_inherited_fds_close_on_exec(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    // SAFETY: this hook performs only direct descriptor syscalls between fork
    // and exec. It neither allocates nor inspects shared process state.
    unsafe {
        command.pre_exec(mark_all_inherited_fds_close_on_exec);
    }
}

#[cfg(not(target_os = "linux"))]
fn mark_inherited_fds_close_on_exec(_command: &mut Command) {}

#[cfg(target_os = "linux")]
fn mark_all_inherited_fds_close_on_exec() -> std::io::Result<()> {
    // SAFETY: `close_range` accepts these integer syscall arguments.
    let result = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            FIRST_INHERITED_FD as libc::c_ulong,
            u32::MAX as libc::c_ulong,
            CLOSE_RANGE_CLOEXEC,
        )
    };
    if result == 0 {
        return Ok(());
    }

    let error = std::io::Error::last_os_error();
    let errno = error.raw_os_error();
    if errno != Some(libc::EINVAL) && errno != Some(libc::ENOSYS) {
        return Err(error);
    }
    mark_inherited_fds_close_on_exec_compat()
}

/// Compatibility path for kernels that predate `CLOSE_RANGE_CLOEXEC`.
/// This scans the live descriptor table after fork, so it covers descriptors
/// above a subsequently lowered `RLIMIT_NOFILE` and has no concurrent opener.
#[cfg(target_os = "linux")]
fn mark_inherited_fds_close_on_exec_compat() -> std::io::Result<()> {
    // SAFETY: the constant is a NUL-terminated path and these flags do not
    // create or modify a filesystem entry.
    let directory = unsafe {
        libc::open(
            PROC_FD_DIRECTORY.as_ptr().cast(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if directory < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let result = mark_proc_descriptors_close_on_exec(directory);
    // SAFETY: `directory` came from `open` above and is still owned here.
    let close_result = unsafe { libc::close(directory) };
    result?;
    if close_result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn mark_proc_descriptors_close_on_exec(directory: libc::c_int) -> std::io::Result<()> {
    let mut buffer = [0_u8; PROC_FD_BUFFER_LEN];
    loop {
        // SAFETY: `buffer` is writable for its full length and `directory` is
        // the live `/proc/self/fd` descriptor opened by the caller.
        let count = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                directory,
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        if count < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let count = usize::try_from(count).map_err(|_| invalid_proc_fd_directory())?;
        if count == 0 {
            return Ok(());
        }
        if count > buffer.len() {
            return Err(invalid_proc_fd_directory());
        }

        let mut offset = 0;
        while offset < count {
            let (record_len, descriptor) = proc_fd_directory_entry(&buffer[..count], offset)?;
            if let Some(descriptor) = descriptor {
                mark_descriptor_close_on_exec(descriptor)?;
            }
            offset = offset
                .checked_add(record_len)
                .ok_or_else(invalid_proc_fd_directory)?;
        }
    }
}

#[cfg(target_os = "linux")]
fn proc_fd_directory_entry(
    buffer: &[u8],
    offset: usize,
) -> std::io::Result<(usize, Option<libc::c_int>)> {
    let header_end = offset
        .checked_add(PROC_DIRENT64_NAME_OFFSET)
        .ok_or_else(invalid_proc_fd_directory)?;
    if header_end > buffer.len() {
        return Err(invalid_proc_fd_directory());
    }
    let record_len = usize::from(u16::from_ne_bytes([
        buffer[offset + PROC_DIRENT64_RECLEN_OFFSET],
        buffer[offset + PROC_DIRENT64_RECLEN_OFFSET + 1],
    ]));
    let record_end = offset
        .checked_add(record_len)
        .ok_or_else(invalid_proc_fd_directory)?;
    if record_len < PROC_DIRENT64_NAME_OFFSET || record_end > buffer.len() {
        return Err(invalid_proc_fd_directory());
    }
    let name_with_padding = &buffer[header_end..record_end];
    let Some(name_end) = name_with_padding.iter().position(|byte| *byte == 0) else {
        return Err(invalid_proc_fd_directory());
    };
    let name = &name_with_padding[..name_end];
    if name == b"." || name == b".." {
        return Ok((record_len, None));
    }
    let mut descriptor = 0 as libc::c_int;
    if name.is_empty() {
        return Err(invalid_proc_fd_directory());
    }
    for byte in name {
        if !byte.is_ascii_digit() {
            return Err(invalid_proc_fd_directory());
        }
        descriptor = descriptor
            .checked_mul(10)
            .and_then(|value| value.checked_add(libc::c_int::from(*byte - b'0')))
            .ok_or_else(invalid_proc_fd_directory)?;
    }
    Ok((
        record_len,
        (descriptor >= FIRST_INHERITED_FD as libc::c_int).then_some(descriptor),
    ))
}

#[cfg(target_os = "linux")]
fn mark_descriptor_close_on_exec(descriptor: libc::c_int) -> std::io::Result<()> {
    // SAFETY: `fcntl` only reads descriptor flags for the candidate fd.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::EBADF) {
            Ok(())
        } else {
            Err(error)
        };
    }
    // SAFETY: `fcntl` updates only the close-on-exec bit on this fd.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn invalid_proc_fd_directory() -> std::io::Error {
    std::io::Error::from_raw_os_error(libc::EIO)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::fs;
    #[cfg(target_os = "linux")]
    use std::net::TcpListener;
    #[cfg(target_os = "linux")]
    use std::os::fd::{AsRawFd, FromRawFd};
    #[cfg(target_os = "linux")]
    use std::time::Duration;

    fn command_arguments(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn seatbelt_uses_the_static_profile_and_path_parameters() {
        let workspace_temp = tempfile::tempdir().expect("temp workspace");
        let scratch_temp = tempfile::tempdir().expect("temp scratch");
        let workspace = workspace_temp
            .path()
            .canonicalize()
            .expect("canonical workspace");
        let scratch = scratch_temp
            .path()
            .canonicalize()
            .expect("canonical scratch");
        for directory in ["home", "cache", "tmp"] {
            std::fs::create_dir(scratch.join(directory)).expect("scratch directory");
        }
        let git_metadata = workspace.join(".git");
        let command = seatbelt_command(
            SeatbeltCommand {
                executable: Path::new(SEATBELT_PATH),
                scratch: &scratch,
                git_metadata: &git_metadata,
                git_metadata_resolved: &git_metadata,
            },
            SandboxLaunch {
                profile: SandboxProfile::WorkspaceNoNetwork,
                workspace: &workspace,
                runtime: &RuntimeRoots::default(),
                env: &[],
            },
            OsStr::new("/usr/bin/true"),
            std::iter::empty::<&str>(),
        );
        let arguments = command_arguments(&command);

        assert_eq!(command.get_program(), OsStr::new(SEATBELT_PATH));
        assert_eq!(command.get_current_dir(), Some(workspace.as_path()));
        let profile = arguments
            .windows(2)
            .find_map(|pair| (pair[0] == "-p").then_some(pair[1].as_str()))
            .expect("-p profile");
        assert_eq!(profile, SEATBELT_PROFILE);
        for definition in [
            format!("-DWORKSPACE={}", workspace.display()),
            format!("-DGIT_METADATA={}", git_metadata.display()),
            format!("-DGIT_METADATA_RESOLVED={}", git_metadata.display()),
            format!("-DSCRATCH={}", scratch.display()),
        ] {
            assert!(arguments.contains(&definition), "{arguments:?}");
        }
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(literal (param \"GIT_METADATA\"))"));
        assert!(profile.contains("(subpath (param \"GIT_METADATA\"))"));
        assert!(profile.contains("(deny file-write-unlink"));
        assert!(!profile.contains("(allow network"));
        assert!(!profile.contains("(allow system-socket"));
    }

    #[test]
    fn seatbelt_resolves_a_gitdir_pointer_file() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let metadata = tempfile::tempdir().expect("external metadata");
        let git_entry = workspace.path().join(".git");
        std::fs::write(
            &git_entry,
            format!("gitdir: {}\n", metadata.path().display()),
        )
        .expect("gitdir pointer");

        assert_eq!(
            seatbelt_git_metadata_path(&git_entry).expect("valid gitdir pointer"),
            metadata.path().canonicalize().expect("canonical metadata")
        );
    }

    #[test]
    fn seatbelt_rejects_an_in_workspace_gitdir_target() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let metadata = workspace.path().join("metadata");
        std::fs::create_dir(&metadata).expect("metadata directory");
        let git_entry = workspace.path().join(".git");
        std::fs::write(&git_entry, "gitdir: metadata\n").expect("gitdir pointer");

        assert_eq!(
            seatbelt_git_metadata_path(&git_entry),
            Err(SandboxUnavailableReason::CannotEnforce)
        );
    }

    #[test]
    fn profile_uses_private_root_workspace_bind_and_network_namespace() {
        let temp = tempfile::tempdir().expect("temp workspace");
        let workspace = temp.path().canonicalize().expect("canonical workspace");
        let command = bwrap_command(
            Path::new("/usr/bin/bwrap"),
            SandboxLaunch {
                profile: SandboxProfile::WorkspaceNoNetwork,
                workspace: &workspace,
                runtime: &RuntimeRoots::default(),
                env: &[],
            },
            OsStr::new("/bin/sh"),
            ["-c", "true"],
        );
        let arguments = command_arguments(&command);

        assert_eq!(command.get_program(), Path::new("/usr/bin/bwrap"));
        assert!(arguments.windows(2).any(|pair| pair == ["--tmpfs", "/"]));
        assert!(arguments.iter().any(|argument| argument == "--unshare-net"));
        assert!(arguments
            .iter()
            .any(|argument| argument == "--disable-userns"));
        assert!(arguments
            .windows(2)
            .any(|pair| pair == ["--cap-drop", "ALL"]));
        assert!(arguments.iter().any(|argument| argument == "--clearenv"));
        assert!(arguments.windows(3).any(|triple| {
            triple
                == [
                    "--bind",
                    workspace.to_string_lossy().as_ref(),
                    SANDBOX_WORKSPACE,
                ]
        }));
        assert!(arguments
            .windows(3)
            .any(|triple| triple == ["--tmpfs", "/tmp", "--dir"]));
        assert!(arguments
            .windows(3)
            .any(|triple| triple == ["--ro-bind", "/usr", "/usr"]));
        assert!(!arguments
            .windows(3)
            .any(|triple| triple == ["--ro-bind", "/", "/"]));
        // `/etc` is mounted read-only on purpose: without it `cc` is a
        // dangling symlink into /etc/alternatives and getpwuid has no passwd
        // file. `/home` is what stays invisible (ADR 0014).
        assert!(arguments
            .windows(3)
            .any(|triple| triple == ["--ro-bind", "/etc", "/etc"]));
        assert!(!arguments
            .windows(3)
            .any(|triple| triple == ["--ro-bind", "/home", "/home"]));
        assert_eq!(
            arguments
                .iter()
                .skip_while(|argument| argument.as_str() != "--")
                .cloned()
                .collect::<Vec<_>>(),
            vec![
                "--",
                "/bin/sh",
                "-c",
                SANDBOX_READY_WRAPPER,
                "euler-sandbox",
                "/bin/sh",
                "-c",
                "true",
            ]
        );
    }

    #[test]
    fn missing_readiness_marker_returns_only_a_safe_reason() {
        let raw_launcher_error = "bwrap: Can't bind mount /home/example/private: permission denied";

        assert_eq!(
            strip_sandbox_ready_marker(raw_launcher_error),
            Err(SandboxUnavailableReason::CannotEnforce)
        );
    }

    #[test]
    fn unavailable_reason_copy_is_safe_and_actionable() {
        assert_eq!(
            SandboxUnavailableReason::BubblewrapMissing.message(),
            "workspace sandbox requires Bubblewrap (`bwrap`) to be installed"
        );
        assert!(
            !SandboxAvailability::Unavailable(SandboxUnavailableReason::CannotEnforce)
                .is_enforced()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn compat_sanitizer_covers_descriptor_above_reduced_soft_limit() {
        let mut limits = std::mem::MaybeUninit::<libc::rlimit>::uninit();
        // SAFETY: `limits` is valid writable storage for this direct syscall.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limits.as_mut_ptr()) },
            0,
            "read descriptor limit"
        );
        // SAFETY: `getrlimit` initialized `limits` above.
        let limits = unsafe { limits.assume_init() };
        if limits.rlim_cur <= 128 || limits.rlim_max < 64 {
            return;
        }

        let source = fs::File::open("/dev/null").expect("open source descriptor");
        // SAFETY: `source` is live, and `F_DUPFD` returns a fresh descriptor
        // at or above 128 on success.
        let duplicated = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD, 128) };
        assert!(duplicated >= 128, "duplicate high descriptor");
        // SAFETY: `F_DUPFD` returned a fresh owned descriptor.
        let high_descriptor = unsafe { fs::File::from_raw_fd(duplicated) };
        let descriptor = high_descriptor.as_raw_fd();
        // SAFETY: `descriptor` is live and this clears only its CLOEXEC bit.
        unsafe {
            let flags = libc::fcntl(descriptor, libc::F_GETFD);
            assert!(flags >= 0, "read high descriptor flags");
            assert_eq!(
                libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC),
                0,
                "clear high descriptor CLOEXEC"
            );
        }

        // Run the post-fork compatibility path in a child so its all-FD
        // mutation cannot affect the concurrently executing test harness.
        // SAFETY: the child only uses the same syscall-only sanitizer that
        // production invokes from `pre_exec`, then exits immediately.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork compatibility probe");
        if child == 0 {
            let reduced = libc::rlimit {
                rlim_cur: 64,
                rlim_max: limits.rlim_max,
            };
            // SAFETY: these calls affect only the child, which exits below.
            let status = unsafe {
                if libc::setrlimit(libc::RLIMIT_NOFILE, &reduced) != 0 {
                    1
                } else if mark_inherited_fds_close_on_exec_compat().is_err() {
                    2
                } else {
                    let flags = libc::fcntl(descriptor, libc::F_GETFD);
                    if flags >= 0 && flags & libc::FD_CLOEXEC != 0 {
                        0
                    } else {
                        3
                    }
                }
            };
            // SAFETY: the child must not run Rust destructors after fork.
            unsafe { libc::_exit(status) };
        }

        let mut status = 0;
        // SAFETY: `child` is the live child created above and `status` is
        // writable storage for its wait status.
        assert_eq!(
            unsafe { libc::waitpid(child, &mut status, 0) },
            child,
            "wait for compatibility probe"
        );
        assert_eq!(status, 0, "compatibility sanitizer child exit status");
    }

    #[test]
    fn detected_toolchain_roots_are_read_only_and_carry_their_variables() {
        let temp = tempfile::tempdir().expect("temp dir");
        let home = temp.path().join("home/example");
        let cargo = home.join(".cargo");
        let rustup = home.join(".rustup");
        std::fs::create_dir_all(cargo.join("bin")).expect("cargo bin");
        std::fs::create_dir_all(&rustup).expect("rustup");
        let unmounted = temp.path().join("elsewhere/bin");
        std::fs::create_dir_all(&unmounted).expect("bin outside every root");
        let path = std::env::join_paths([cargo.join("bin"), unmounted.clone()]).expect("join PATH");
        let runtime = RuntimeRoots::from_environment(
            Some(home.clone()),
            |name| (name == "CARGO_HOME").then(|| cargo.clone().into_os_string()),
            Some(path),
        );
        let home = home.canonicalize().expect("canonical home");
        let cargo = cargo.canonicalize().expect("canonical cargo home");
        let rustup = rustup.canonicalize().expect("canonical rustup home");

        assert!(runtime.roots.contains(&cargo), "{runtime:?}");
        assert!(runtime.roots.contains(&rustup), "{runtime:?}");
        assert!(runtime
            .variables
            .contains(&(OsString::from("CARGO_HOME"), cargo.clone().into_os_string())));
        assert!(runtime
            .variables
            .contains(&(OsString::from("RUSTUP_HOME"), rustup.into_os_string())));
        // The host PATH is the evidence for what a mounted root provides;
        // an entry outside every root never enters the sandbox PATH.
        assert_eq!(runtime.path_entries, vec![cargo.join("bin")]);
        let sandbox_path = runtime.sandbox_path().to_string_lossy().into_owned();
        assert!(
            sandbox_path.ends_with(SYSTEM_SANDBOX_PATH),
            "{sandbox_path}"
        );
        assert!(
            !sandbox_path.contains(unmounted.to_string_lossy().as_ref()),
            "{sandbox_path}"
        );
        // The fixture home is a temp directory, which on Linux lives under
        // the profile's own `/tmp` and is therefore deliberately left alone —
        // remounting it would shadow the sandbox home and cache. Assert the
        // read-only-parent rule on a home where it applies.
        let _ = home;
        let elsewhere = RuntimeRoots {
            roots: vec![PathBuf::from("/home/example/.cargo")],
            variables: Vec::new(),
            path_entries: Vec::new(),
            home: Some(PathBuf::from("/home/example")),
            workspace: None,
        };
        assert!(elsewhere
            .read_only_parents()
            .contains(&PathBuf::from("/home/example")));
    }

    #[test]
    fn the_real_home_is_never_a_toolchain_root() {
        let temp = tempfile::tempdir().expect("temp dir");
        let home = temp.path().join("home/example");
        std::fs::create_dir_all(&home).expect("home");
        let runtime = RuntimeRoots::from_environment(
            Some(home.clone()),
            |name| (name == "CARGO_HOME").then(|| home.clone().into_os_string()),
            None,
        );

        let home = home.canonicalize().expect("canonical home");
        assert!(!runtime.roots.contains(&home), "{runtime:?}");
        assert!(runtime
            .variables
            .iter()
            .all(|(name, _)| name != "CARGO_HOME"));
    }

    /// Rust's official images put `CARGO_HOME` at `/usr/local/cargo`, inside
    /// the system runtime the profile already binds. Mounting it again would
    /// shadow that bind; dropping its variable and `PATH` makes `cargo`
    /// command-not-found while the session still records `bwrap`.
    #[test]
    fn a_toolchain_inside_the_system_runtime_keeps_its_variable_without_a_second_mount() {
        let usr_local = Path::new("/usr/local");
        if !usr_local.is_dir() {
            return;
        }
        let cargo = usr_local.join("cargo");
        let exists = cargo.is_dir();
        let runtime = RuntimeRoots::from_environment(
            None,
            |name| (name == "CARGO_HOME" && exists).then(|| cargo.clone().into_os_string()),
            exists.then(|| cargo.join("bin").into_os_string()),
        );
        if !exists {
            return;
        }

        let cargo = cargo.canonicalize().expect("canonical cargo home");
        assert!(!runtime.roots.contains(&cargo), "{runtime:?}");
        assert!(runtime
            .variables
            .contains(&(OsString::from("CARGO_HOME"), cargo.clone().into_os_string())));
        assert!(runtime.path_entries.contains(&cargo.join("bin")));
    }

    /// The containerized Rust images put `CARGO_HOME` under `/usr/local`,
    /// where the wholesale `/usr` bind carries it and no root of Euler's own
    /// is mounted. Masking only mounted roots would leave the token readable
    /// in exactly that common case.
    #[test]
    fn credentials_are_masked_in_a_toolchain_home_the_system_runtime_carries() {
        let cargo = PathBuf::from("/usr/local/cargo");
        let runtime = RuntimeRoots {
            roots: Vec::new(),
            variables: vec![(OsString::from("CARGO_HOME"), cargo.clone().into_os_string())],
            path_entries: Vec::new(),
            home: None,
            workspace: None,
        };

        assert_eq!(runtime.cargo_home().as_deref(), Some(cargo.as_path()));
        assert!(runtime.roots.is_empty(), "{runtime:?}");
    }

    /// Cargo accepts a registry token in `config.toml` too, which is not
    /// masked because a build needs that file. Say so rather than hide it.
    #[test]
    fn a_registry_token_in_config_toml_is_reported_not_masked() {
        let temp = tempfile::tempdir().expect("temp dir");
        let cargo = temp.path().join("home/example/.cargo");
        std::fs::create_dir_all(&cargo).expect("cargo home");
        std::fs::write(
            cargo.join("config.toml"),
            "[build]\njobs = 4\n\n[registries.internal]\nindex = \"https://example.invalid\"\n\
token = \"secret\"\n",
        )
        .expect("config");
        let runtime = RuntimeRoots::from_environment(
            Some(temp.path().join("home/example")),
            |name| (name == "CARGO_HOME").then(|| cargo.clone().into_os_string()),
            None,
        );

        let reported = runtime.config_files_holding_a_registry_token();
        assert_eq!(reported.len(), 1, "{reported:?}");
        assert!(reported[0].ends_with("config.toml"), "{reported:?}");

        let mut command = Command::new("/usr/bin/bwrap");
        add_runtime_root_mounts(&mut command, &runtime);
        let arguments = command_arguments(&command);
        let config = reported[0].to_string_lossy().into_owned();
        // Reported, never masked: masking it would take the registry sources
        // and build settings with it.
        assert!(!arguments.contains(&config), "{arguments:?}");
    }

    /// Deliberately over-broad: every spelling Cargo accepts must be caught,
    /// and there is no TOML parser here to tell them apart.
    #[test]
    fn every_spelling_of_a_token_key_is_reported() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = temp.path().join("config.toml");
        for declaration in [
            "[registry]\ntoken = \"secret\"\n",
            "registry.token = \"secret\"\n",
            "registry = { token = \"secret\" }\n",
            "[\"registries\".\"internal\"]\ntoken = \"secret\"\n",
            "[registries.internal]\n  token   =   \"secret\"\n",
        ] {
            std::fs::write(&config, declaration).expect("config");
            assert!(
                file_declares_a_token(&config),
                "missed a token in {declaration:?}"
            );
        }
    }

    /// Cargo still reads the extensionless config, so it is scanned too.
    #[test]
    fn the_legacy_cargo_config_filename_is_scanned() {
        let temp = tempfile::tempdir().expect("temp dir");
        let cargo = temp.path().join("home/example/.cargo");
        std::fs::create_dir_all(&cargo).expect("cargo home");
        std::fs::write(cargo.join("config"), "[registry]\ntoken = \"secret\"\n")
            .expect("legacy config");
        let runtime = RuntimeRoots::from_environment(
            Some(temp.path().join("home/example")),
            |name| (name == "CARGO_HOME").then(|| cargo.clone().into_os_string()),
            None,
        );

        let reported = runtime.config_files_holding_a_registry_token();
        assert_eq!(reported.len(), 1, "{reported:?}");
        assert!(reported[0].ends_with("config"), "{reported:?}");
    }

    #[test]
    fn a_config_without_a_registry_token_is_not_reported() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = temp.path().join("config.toml");
        std::fs::write(
            &config,
            "[build]\njobs = 4\n\n[registries.internal]\nindex = \"https://example.invalid\"\n",
        )
        .expect("config");
        assert!(!file_declares_a_token(&config));

        // A commented-out declaration is not one.
        std::fs::write(&config, "# token = \"secret\"\n").expect("config");
        assert!(!file_declares_a_token(&config));
        // A key that merely mentions the word is not a token key.
        std::fs::write(&config, "[build]\ntokenizer = \"x\"\n").expect("config");
        assert!(!file_declares_a_token(&config));
    }

    /// The official Go images set `GOPATH=/go`. Rejecting a two-component
    /// explicit root leaves the toolchain unusable while the session records
    /// `bwrap`.
    #[test]
    fn an_explicit_toolchain_variable_may_name_a_two_component_root() {
        // `/usr` stands in for `/go`: two components, and a real directory
        // on both platforms so canonicalization does not change its depth.
        let two_component = Path::new("/usr");

        assert!(
            usable_runtime_root(two_component, None, true).is_some(),
            "an explicit two-component root is an answer"
        );
        assert!(
            usable_runtime_root(two_component, None, false).is_none(),
            "a guessed two-component root is not"
        );
        // Neither form may be the home itself, or `/`.
        assert!(usable_runtime_root(two_component, Some(two_component), true).is_none());
        assert!(usable_runtime_root(Path::new("/"), None, true).is_none());
    }

    /// A container that mounts the project at `/workspace` has a real
    /// `CARGO_HOME=/workspace/.cargo`. Rejecting it for colliding with the
    /// sandbox workspace path would leave cargo missing while the session
    /// records `bwrap`.
    #[test]
    fn a_toolchain_home_under_a_host_workspace_named_workspace_survives() {
        let workspace = Path::new(SANDBOX_WORKSPACE);
        let cargo = workspace.join(".cargo");

        // Detection no longer rules it out: whether it collides depends on
        // where the host workspace is, which detection does not know.
        assert!(!names_a_profile_mount_point(&cargo));
        assert!(!shadowed_by_the_workspace_bind(&cargo, workspace));
        // The same path is shadowed when the workspace is somewhere else.
        assert!(shadowed_by_the_workspace_bind(
            &cargo,
            Path::new("/home/example/project")
        ));

        let runtime = RuntimeRoots {
            roots: vec![cargo.clone()],
            variables: vec![(OsString::from("CARGO_HOME"), cargo.clone().into_os_string())],
            path_entries: vec![cargo.join("bin")],
            home: None,
            workspace: None,
        }
        .excluding(workspace);

        // No second mount — the workspace bind carries it — but the variable
        // and PATH survive, already at their bound path.
        assert!(runtime.roots.is_empty(), "{runtime:?}");
        let exported = sandbox_environment(&runtime, &[]);
        assert!(
            exported.contains(&(
                OsString::from("CARGO_HOME"),
                OsString::from("/workspace/.cargo")
            )),
            "{exported:?}"
        );
        let sandbox_path = runtime.sandbox_path().to_string_lossy().into_owned();
        assert!(
            sandbox_path.starts_with("/workspace/.cargo/bin:"),
            "{sandbox_path}"
        );
    }

    /// The mirror image: something occupying the sandbox workspace path that
    /// is not the workspace would simply vanish under the bind.
    #[test]
    fn a_root_shadowed_by_the_workspace_bind_is_dropped() {
        let runtime = RuntimeRoots {
            roots: vec![PathBuf::from("/workspace/cargo")],
            variables: vec![(
                OsString::from("CARGO_HOME"),
                OsString::from("/workspace/cargo"),
            )],
            path_entries: vec![PathBuf::from("/workspace/cargo/bin")],
            home: None,
            workspace: None,
        }
        .excluding(Path::new("/home/example/project"));

        assert!(runtime.roots.is_empty(), "{runtime:?}");
        assert!(runtime.variables.is_empty(), "{runtime:?}");
        assert!(runtime.path_entries.is_empty(), "{runtime:?}");
    }

    /// A root nested inside another needs one mount and both variables:
    /// dropping `RUSTUP_HOME` breaks every rustup proxy.
    /// An explicit variable naming one of the profile's own mount points
    /// would bind over the private `/tmp` the sandbox home, cache and TMPDIR
    /// live in, and the resulting probe failure would be blamed on /usr.
    #[test]
    fn a_toolchain_variable_may_not_name_a_profile_mount_point() {
        for mount in ["/tmp", "/proc", "/dev"] {
            if !Path::new(mount).is_dir() {
                continue;
            }
            assert!(
                usable_runtime_root(Path::new(mount), None, true).is_none(),
                "{mount} was accepted as a toolchain root"
            );
        }
        let runtime = RuntimeRoots::from_environment(
            None,
            |name| (name == "GOPATH").then(|| OsString::from("/tmp")),
            None,
        );
        assert!(runtime.roots.is_empty(), "{runtime:?}");
        assert!(runtime.variables.is_empty(), "{runtime:?}");

        // The predicate itself, not `usable_runtime_root`: these paths do not
        // exist on the host, so canonicalization would reject them anyway and
        // the assertion would still pass with the predicate deleted.
        assert!(names_a_profile_mount_point(Path::new(SANDBOX_HOME)));
        assert!(names_a_profile_mount_point(Path::new(SANDBOX_CACHE)));
        assert!(names_a_profile_mount_point(Path::new("/proc/1")));
        assert!(names_a_profile_mount_point(Path::new("/dev")));
        // `/tmp` is caught only by being an ancestor of the sandbox home.
        assert!(names_a_profile_mount_point(Path::new("/tmp")));

        // A root merely *inside* the private /tmp is not a collision: the
        // profile lays its tmpfs down first, so the bind shadows nothing.
        // This is the direct-child case the rule turns on.
        assert!(!names_a_profile_mount_point(Path::new("/tmp/cargo")));
        assert!(!names_a_profile_mount_point(Path::new("/tmp/home-of-mine")));
        let temp = tempfile::tempdir().expect("temp dir");
        let inside = temp.path().join("cargo");
        std::fs::create_dir(&inside).expect("cargo home");
        assert!(usable_runtime_root(&inside, None, true).is_some());
    }

    /// A per-project toolchain home is not dropped: the directory really is
    /// there, bound read-write, so unsetting the variable would leave the
    /// toolchain in plain sight and unusable.
    #[test]
    fn a_toolchain_home_inside_the_workspace_is_re_pointed_not_dropped() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        let cargo = workspace.join(".cargo");
        std::fs::create_dir_all(cargo.join("bin")).expect("workspace cargo home");
        let path = std::env::join_paths([cargo.join("bin")]).expect("join PATH");
        let workspace = workspace.canonicalize().expect("canonical workspace");
        let runtime = RuntimeRoots::from_environment(
            Some(temp.path().to_path_buf()),
            |name| (name == "CARGO_HOME").then(|| cargo.clone().into_os_string()),
            Some(path),
        )
        .excluding(&workspace);

        // No second mount: the workspace bind already carries it.
        assert!(runtime.roots.is_empty(), "{runtime:?}");
        let exported = sandbox_environment(&runtime, &[]);
        assert!(
            exported.contains(&(
                OsString::from("CARGO_HOME"),
                OsString::from("/workspace/.cargo")
            )),
            "{exported:?}"
        );
        let sandbox_path = runtime.sandbox_path().to_string_lossy().into_owned();
        assert!(
            sandbox_path.starts_with("/workspace/.cargo/bin:"),
            "{sandbox_path}"
        );
        // Nothing in the workspace is masked or reported: it is the user's own
        // project, readable by design.
        assert!(runtime.cargo_home().is_none(), "{runtime:?}");
    }

    #[test]
    fn a_nested_toolchain_root_keeps_its_variable_and_loses_only_its_mount() {
        let temp = tempfile::tempdir().expect("temp dir");
        let cargo = temp.path().join("home/example/.cargo");
        let rustup = cargo.join("rustup");
        std::fs::create_dir_all(&rustup).expect("nested rustup home");
        let runtime = RuntimeRoots::from_environment(
            Some(temp.path().join("home/example")),
            |name| match name {
                "CARGO_HOME" => Some(cargo.clone().into_os_string()),
                "RUSTUP_HOME" => Some(rustup.clone().into_os_string()),
                _ => None,
            },
            None,
        );
        let cargo = cargo.canonicalize().expect("canonical cargo home");
        let rustup = rustup.canonicalize().expect("canonical rustup home");

        assert_eq!(runtime.roots, vec![cargo.clone()]);
        assert!(runtime
            .variables
            .contains(&(OsString::from("CARGO_HOME"), cargo.into_os_string())));
        assert!(runtime
            .variables
            .contains(&(OsString::from("RUSTUP_HOME"), rustup.into_os_string())));
    }

    /// A toolchain home under the profile's own `/tmp` must not put a second
    /// tmpfs over it and then remount it read-only: the sandbox home, cache
    /// and TMPDIR all live there.
    #[test]
    fn a_toolchain_root_under_a_profile_mount_point_never_remounts_it() {
        let temp = tempfile::tempdir().expect("temp dir");
        let cargo = temp.path().join("cargo");
        std::fs::create_dir_all(&cargo).expect("cargo home");
        let mut runtime = RuntimeRoots::from_environment(
            Some(PathBuf::from("/tmp")),
            |name| (name == "CARGO_HOME").then(|| cargo.clone().into_os_string()),
            None,
        );
        // Stand in for a host whose temporary directory is literally `/tmp`.
        runtime.roots = vec![PathBuf::from("/tmp/cargo")];
        runtime.home = Some(PathBuf::from("/tmp"));

        let parents = runtime.read_only_parents();

        assert!(
            !parents.iter().any(|parent| parent == Path::new("/tmp")),
            "{parents:?}"
        );
    }

    /// Mounting a toolchain home read-only makes files inside it readable that
    /// `main` returned ENOENT for. The registry token is one of them.
    #[test]
    fn cargo_credentials_are_masked_inside_a_mounted_toolchain_home() {
        let temp = tempfile::tempdir().expect("temp dir");
        let cargo = temp.path().join("home/example/.cargo");
        std::fs::create_dir_all(&cargo).expect("cargo home");
        std::fs::write(cargo.join("credentials.toml"), "token = \"secret\"").expect("token");
        std::fs::write(cargo.join("config.toml"), "[net]").expect("config");
        let runtime = RuntimeRoots::from_environment(
            Some(temp.path().join("home/example")),
            |name| (name == "CARGO_HOME").then(|| cargo.clone().into_os_string()),
            None,
        );
        let mut command = Command::new("/usr/bin/bwrap");
        add_runtime_root_mounts(&mut command, &runtime);
        let arguments = command_arguments(&command);
        let cargo = cargo.canonicalize().expect("canonical cargo home");
        let credentials = cargo
            .join("credentials.toml")
            .to_string_lossy()
            .into_owned();
        let config = cargo.join("config.toml").to_string_lossy().into_owned();

        assert!(
            arguments
                .windows(3)
                .any(|triple| triple == ["--ro-bind-try", "/dev/null", credentials.as_str()]),
            "{arguments:?}"
        );
        // A build needs the config; only the credential files are covered.
        assert!(!arguments.contains(&config), "{arguments:?}");
    }

    /// `/etc` is not optional on Debian and Ubuntu: `cc` is a symlink into
    /// `/etc/alternatives` and `getpwuid` needs `/etc/passwd`.
    #[test]
    fn the_profile_mounts_the_system_runtime_a_toolchain_actually_needs() {
        assert!(RUNTIME_MOUNTS.contains(&"/etc"), "{RUNTIME_MOUNTS:?}");
        assert!(RUNTIME_MOUNTS.contains(&"/opt"), "{RUNTIME_MOUNTS:?}");
        for entry in ["/usr/local/bin", "/usr/sbin"] {
            assert!(
                SYSTEM_SANDBOX_PATH.split(':').any(|part| part == entry),
                "{SYSTEM_SANDBOX_PATH}"
            );
        }
    }

    /// The two probes must agree, or a host that passes the trivial one and
    /// fails the profile is told to fix the wrong thing.
    #[test]
    fn both_probes_use_the_same_isolation_flags() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().canonicalize().expect("canonical workspace");
        let command = bwrap_command(
            Path::new("/usr/bin/bwrap"),
            SandboxLaunch {
                profile: SandboxProfile::WorkspaceNoNetwork,
                workspace: &workspace,
                runtime: &RuntimeRoots::default(),
                env: &[],
            },
            OsStr::new("/bin/sh"),
            ["-c", "true"],
        );
        let arguments = command_arguments(&command);

        for flag in PROFILE_ISOLATION_FLAGS {
            assert!(
                arguments.iter().any(|argument| argument == flag),
                "{flag} missing from {arguments:?}"
            );
        }
    }

    /// Every reason has to name something the user can act on; mapping three
    /// of them onto the user-namespace text sends people to the wrong sysctl.
    #[test]
    fn each_unavailable_reason_maps_to_its_own_cause() {
        assert_eq!(
            SandboxFailureCause::for_reason(SandboxUnavailableReason::InvalidWorkspace),
            SandboxFailureCause::InvalidWorkspace
        );
        assert_eq!(
            SandboxFailureCause::for_reason(SandboxUnavailableReason::UnsupportedPlatform),
            SandboxFailureCause::UnsupportedPlatform
        );
        assert_eq!(
            SandboxFailureCause::for_reason(SandboxUnavailableReason::BubblewrapMissing),
            SandboxFailureCause::BubblewrapMissing
        );
        assert!(SandboxFailureCause::BubblewrapTooOld
            .remedy()
            .contains("0.8.0"));
    }

    #[test]
    fn a_toolchain_root_inside_the_workspace_is_not_also_mounted_read_only() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        let cargo = workspace.join(".cargo");
        std::fs::create_dir_all(&cargo).expect("workspace cargo home");
        let runtime = RuntimeRoots::from_environment(
            Some(temp.path().to_path_buf()),
            |name| (name == "CARGO_HOME").then(|| cargo.clone().into_os_string()),
            None,
        )
        .excluding(&workspace.canonicalize().expect("canonical workspace"));

        assert!(runtime.roots.is_empty(), "{runtime:?}");
    }

    #[test]
    fn toolchain_roots_are_bound_read_only_between_a_tmpfs_and_its_remount() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        // Synthetic roots, not a temp directory: on Linux a temp directory
        // lives under the profile's own `/tmp`, which is deliberately never
        // remounted, so the ordering under test would not be emitted at all.
        let home = PathBuf::from("/home/example");
        let cargo = home.join(".cargo");
        let runtime = RuntimeRoots {
            roots: vec![cargo.clone()],
            variables: vec![(OsString::from("CARGO_HOME"), cargo.clone().into_os_string())],
            path_entries: Vec::new(),
            home: Some(home.clone()),
            workspace: None,
        };
        let command = bwrap_command(
            Path::new("/usr/bin/bwrap"),
            SandboxLaunch {
                profile: SandboxProfile::WorkspaceNoNetwork,
                workspace: &workspace.canonicalize().expect("canonical workspace"),
                runtime: &runtime,
                env: &[],
            },
            OsStr::new("/bin/sh"),
            ["-c", "true"],
        );
        let arguments = command_arguments(&command);
        let home = home.to_string_lossy().into_owned();
        let cargo = cargo.to_string_lossy().into_owned();

        let tmpfs = arguments
            .windows(2)
            .position(|pair| pair == ["--tmpfs", home.as_str()])
            .expect("holding directory is a mount point");
        let bind = arguments
            .windows(3)
            .position(|triple| triple == ["--ro-bind", cargo.as_str(), cargo.as_str()])
            .expect("toolchain root is bound read-only");
        let remount = arguments
            .windows(2)
            .position(|pair| pair == ["--remount-ro", home.as_str()])
            .expect("holding directory is remounted read-only");
        assert!(tmpfs < bind && bind < remount, "{arguments:?}");
        assert!(arguments
            .windows(2)
            .any(|pair| pair == ["CARGO_HOME", cargo.as_str()]));
    }

    #[test]
    fn an_unavailable_backend_names_a_cause_and_a_way_out() {
        let status = SandboxStatus::Unavailable {
            reason: SandboxUnavailableReason::BubblewrapMissing,
            cause: SandboxFailureCause::BubblewrapMissing,
        };
        let diagnostic = status.diagnostic().expect("diagnostic");

        assert_eq!(status.backend_label(), "unavailable");
        assert!(
            diagnostic.contains("`bwrap` is not installed"),
            "{diagnostic}"
        );
        assert!(
            diagnostic.contains("sudo apt install bubblewrap"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("fail closed"), "{diagnostic}");
        // The probe result is fixed for the session's lifetime, so say so
        // rather than let a user fix the host and wonder why nothing changed.
        assert!(
            diagnostic.contains("cached for this process"),
            "{diagnostic}"
        );
        assert!(SandboxStatus::Host.diagnostic().is_none());
        assert_eq!(SandboxStatus::Host.backend_label(), "host");
        assert_eq!(
            SandboxStatus::Enforced(SandboxBackend::Bwrap).backend_label(),
            "bwrap"
        );
        assert_eq!(
            SandboxStatus::Enforced(SandboxBackend::Seatbelt).backend_label(),
            "seatbelt"
        );
    }

    #[test]
    fn every_failure_cause_names_a_distinct_remedy() {
        for cause in [
            SandboxFailureCause::BubblewrapMissing,
            SandboxFailureCause::SeatbeltMissing,
            SandboxFailureCause::GitMetadataSymlink,
            SandboxFailureCause::UserNamespacesDisabled,
            SandboxFailureCause::AppArmorUserNamespaceRestriction,
            SandboxFailureCause::Container,
            SandboxFailureCause::Wsl1,
            SandboxFailureCause::SeatbeltProfileRejected,
            SandboxFailureCause::Unattributed,
        ] {
            assert!(!cause.description().is_empty());
            assert!(!cause.remedy().is_empty());
        }
    }

    #[test]
    fn the_platform_default_is_enforced_on_linux_and_macos() {
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert_eq!(
                SubprocessSandbox::default(),
                SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork)
            );
        } else {
            assert_eq!(SubprocessSandbox::default(), SubprocessSandbox::Host);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn detected_toolchains_run_inside_the_sandbox_and_the_real_home_stays_read_only() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let runtime = RuntimeRoots::detect();
        let sandbox = WorkspaceSandbox::with_runtime_roots(
            &workspace,
            SandboxProfile::WorkspaceNoNetwork,
            runtime.clone(),
        );
        if !sandbox.availability().is_enforced() {
            return;
        }
        let Some(home) = runtime.home.clone() else {
            return;
        };

        // A write anywhere under the real home must fail, not land in a
        // private copy that is silently discarded.
        let script = format!(
            "if printf bad > {}/euler-must-not-write; then exit 1; fi",
            shell_quote(&home)
        );
        let output = sandbox
            .command("/bin/sh", ["-c", script.as_str()], &[])
            .expect("sandbox command")
            .output()
            .expect("run sandbox command");
        assert!(output.status.success(), "{output:?}");
        assert!(!home.join("euler-must-not-write").exists());

        // Toolchains the host environment implies stay reachable.
        for tool in ["cargo", "node"] {
            if !runtime
                .path_entries
                .iter()
                .any(|entry| entry.join(tool).is_file())
            {
                continue;
            }
            let output = sandbox
                .command("/bin/sh", ["-c", format!("{tool} --version").as_str()], &[])
                .expect("sandbox command")
                .output()
                .expect("run sandbox command");
            assert!(output.status.success(), "{tool} in sandbox: {output:?}");
        }
    }

    #[test]
    fn invalid_workspace_fails_closed_before_bubblewrap_is_invoked() {
        let temp = tempfile::tempdir().expect("temp dir");
        let missing = temp.path().join("missing");
        let sandbox = WorkspaceSandbox::new(&missing, SandboxProfile::WorkspaceNoNetwork);

        // Fails closed on every platform — that is the guarantee under test.
        // The reason is platform-specific: off Linux the platform check
        // short-circuits before the workspace is validated (ADR 0014).
        #[cfg(target_os = "linux")]
        let expected = SandboxUnavailableReason::InvalidWorkspace;
        #[cfg(not(target_os = "linux"))]
        let expected = SandboxUnavailableReason::UnsupportedPlatform;

        assert_eq!(
            sandbox.availability(),
            SandboxAvailability::Unavailable(expected)
        );
        assert_eq!(
            sandbox.command("/bin/sh", ["-c", "true"], &[]).map(|_| ()),
            Err(expected)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn enforced_profile_can_write_workspace_but_not_outside_it() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&outside).expect("outside");
        let secret = outside.join("secret");
        fs::write(&secret, "do-not-expose").expect("plant secret");
        let escape = outside.join("escape");
        let sandbox = WorkspaceSandbox::new(&workspace, SandboxProfile::WorkspaceNoNetwork);
        if !sandbox.availability().is_enforced() {
            return;
        }

        let secret = shell_quote(&secret);
        let escape = shell_quote(&escape);
        let script = format!(
            "printf inside > /workspace/inside.txt; test ! -e {secret}; if echo outside > {escape}; then exit 1; fi"
        );
        let output = sandbox
            .command("/bin/sh", ["-c", script.as_str()], &[])
            .expect("enforced sandbox command")
            .output()
            .expect("run sandbox command");

        assert!(
            output.status.success(),
            "sandboxed shell failed: {output:?}"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("inside.txt")).unwrap(),
            "inside"
        );
        assert!(!outside.join("escape").exists());
    }

    /// Prevents "linker `cc` not found" for every crate that links.
    ///
    /// On Debian and Ubuntu `cc` in /usr/bin is a symlink into
    /// /etc/alternatives, so without /etc mounted it dangles and no crate
    /// that links can build. Only a real compile-and-link exercises that;
    /// `cargo --version` runs happily with a dangling `cc`.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_c_program_compiles_links_and_runs_inside_the_sandbox() {
        // A bare container may have no compiler at all; that is not a
        // sandbox failure. `symlink_metadata` deliberately: `cc` is itself a
        // symlink into /etc/alternatives, so `exists` would follow it and
        // report "no compiler" on exactly the broken-/etc host this test
        // exists to catch.
        if fs::symlink_metadata("/usr/bin/cc").is_err() {
            return;
        }
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let sandbox = WorkspaceSandbox::new(&workspace, SandboxProfile::WorkspaceNoNetwork);
        if !sandbox.availability().is_enforced() {
            eprintln!("skipped: the sandbox is unavailable on this host");
            return;
        }
        fs::write(
            workspace.join("hello.c"),
            "#include <stdio.h>\nint main(void) { printf(\"linked-and-ran\\n\"); return 0; }\n",
        )
        .expect("source file");

        let output = sandbox
            .command("/bin/sh", ["-c", "cc hello.c -o hello && ./hello"], &[])
            .expect("sandbox command")
            .output()
            .expect("run sandbox command");

        assert!(
            output.status.success(),
            "compile and link failed inside the sandbox: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("linked-and-ran"),
            "{output:?}"
        );
    }

    /// Prevents a sandbox with a uid but no user name.
    ///
    /// `getpwuid` reads /etc/passwd. Without it anything that resolves the
    /// current user — git's fallback identity, ssh, a build script asking who
    /// it runs as — fails, and the uid maps unchanged so the answer must be
    /// the host's own user.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_current_user_resolves_by_name_inside_the_sandbox() {
        let Ok(host) = Command::new("/usr/bin/id").arg("-un").output() else {
            return;
        };
        if !host.status.success() {
            return;
        }
        let host = String::from_utf8_lossy(&host.stdout).trim().to_owned();
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let sandbox = WorkspaceSandbox::new(&workspace, SandboxProfile::WorkspaceNoNetwork);
        if !sandbox.availability().is_enforced() {
            eprintln!("skipped: the sandbox is unavailable on this host");
            return;
        }

        let output = sandbox
            .command("/bin/sh", ["-c", "id -un"], &[])
            .expect("sandbox command")
            .output()
            .expect("run sandbox command");

        assert!(
            output.status.success(),
            "resolving the current user failed inside the sandbox: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Trimmed equality, not `contains`: a sandbox that resolved every
        // uid to `nobody` would pass a substring check whenever the host name
        // happened to be a substring of the answer.
        let sandboxed = String::from_utf8_lossy(&output.stdout);
        let sandboxed = strip_sandbox_ready_marker(&sandboxed)
            .expect("sandbox readiness marker")
            .trim();
        assert_eq!(
            sandboxed, host,
            "sandbox user name does not match the host's"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn enforced_profile_cannot_connect_to_a_host_listener() {
        if !Path::new("/usr/bin/python3").is_file() {
            return;
        }
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let sandbox = WorkspaceSandbox::new(&workspace, SandboxProfile::WorkspaceNoNetwork);
        if !sandbox.availability().is_enforced() {
            return;
        }
        let listener = TcpListener::bind("127.0.0.1:0").expect("host listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let port = listener.local_addr().expect("listener address").port();
        let script =
            format!("import socket; socket.create_connection(('127.0.0.1', {port}), timeout=1)");
        let output = sandbox
            .command("/usr/bin/python3", ["-c", script.as_str()], &[])
            .expect("enforced sandbox command")
            .output()
            .expect("run sandbox command");

        assert!(
            !output.status.success(),
            "sandbox unexpectedly reached host network"
        );
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            listener.accept().is_err(),
            "host listener received a connection"
        );
    }
}

#[cfg(test)]
#[path = "sandbox_test.rs"]
mod sandbox_test;
