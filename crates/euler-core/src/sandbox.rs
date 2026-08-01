//! Linux workspace subprocess sandboxing with Bubblewrap.
//!
//! The profile is deliberately narrow: an agent-controlled child sees its
//! explicit writable roots, a private runtime, no host home, and no host
//! network namespace. This is an execution boundary, not a synonym for
//! permission approval.

use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The first Linux profile Euler intends to advertise to users.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxProfile {
    /// A writable workspace with no host home or network namespace access.
    WorkspaceNoNetwork,
}

impl SandboxProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceNoNetwork => "workspace-no-network",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::WorkspaceNoNetwork => "sandboxed workspace (network disabled)",
        }
    }
}

/// Whether agent-controlled subprocesses use a sandbox profile.
///
/// This is a core execution choice, intentionally separate from the
/// capability gate and its approval modes. The default is the enforced
/// no-network profile; Disabled blocks subprocess tools.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubprocessSandbox {
    /// Agent-controlled subprocesses are unavailable. This is fail-closed,
    /// not permission to run directly on the host.
    Disabled,
    Enforce(SandboxProfile),
}

impl Default for SubprocessSandbox {
    fn default() -> Self {
        Self::Enforce(SandboxProfile::WorkspaceNoNetwork)
    }
}

/// A concise, non-secret reason why a requested sandbox profile cannot run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxUnavailableReason {
    /// Euler is not running on a Linux host.
    UnsupportedPlatform,
    /// The host has no implemented mount-identity boundary for structured
    /// filesystem access.
    StructuredPathUnsupportedPlatform,
    /// The `bwrap` executable was not found.
    BubblewrapMissing,
    /// Bubblewrap could not create the profile that Euler requires.
    CannotEnforce,
    /// The selected workspace cannot be resolved to a directory.
    InvalidWorkspace,
    /// The requested writable-root set is ambiguous or too broad to mount.
    InvalidWritableRoots,
    /// The requested read-only runtime-root set is ambiguous or too broad.
    InvalidRuntimeRoots,
    /// Euler could not completely inspect every host-backed mount source.
    AuthorityInspectionFailed,
    /// A host IPC or device node would cross the filesystem boundary.
    UnsafeSpecialNode,
    /// A pseudo filesystem or nested mount would bypass ordinary path rules.
    UnsafeMountTopology,
}

impl SandboxUnavailableReason {
    pub const fn message(self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => "workspace sandbox is currently supported on Linux only",
            Self::StructuredPathUnsupportedPlatform => {
                "structured workspace authority is currently supported on Linux and macOS only"
            }
            Self::BubblewrapMissing => {
                "workspace sandbox requires Bubblewrap (`bwrap`) to be installed"
            }
            Self::CannotEnforce => {
                "this host cannot enforce Euler's required workspace sandbox profile"
            }
            Self::InvalidWorkspace => {
                "workspace sandbox requires an accessible workspace directory"
            }
            Self::InvalidWritableRoots => {
                "workspace sandbox requires distinct, non-overlapping writable directories"
            }
            Self::InvalidRuntimeRoots => {
                "workspace sandbox requires distinct, non-overlapping read-only runtime directories"
            }
            Self::AuthorityInspectionFailed => {
                "workspace authority could not completely inspect its host-backed roots"
            }
            Self::UnsafeSpecialNode => {
                "workspace sandbox roots must not contain host IPC or device nodes"
            }
            Self::UnsafeMountTopology => {
                "workspace authority roots must use an approved ordinary filesystem and contain no nested mounts"
            }
        }
    }
}

impl fmt::Display for SandboxUnavailableReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
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

/// A workspace-specific profile whose host-root identities are frozen at
/// construction and whose complete Bubblewrap probe is cached on first use.
/// All bind sources are walked during that probe and again at the final launch
/// boundary. It retains that stable availability result so
/// callers can fail closed without copying raw launcher diagnostics into tool
/// output or provenance.
#[derive(Clone, Debug)]
pub(crate) struct WorkspaceSandbox {
    writable_roots: Vec<PathBuf>,
    runtime_roots: Vec<PathBuf>,
    system_runtime_mounts: Vec<SystemRuntimeMount>,
    authority_mounts: Vec<AuthorityMount>,
    bwrap: Option<PathBuf>,
    profile: SandboxProfile,
    availability: std::sync::OnceLock<SandboxAvailability>,
}

/// A Bubblewrap launcher plus output channels owned exclusively by the inner
/// agent command. Bubblewrap's ordinary stdout and stderr are deliberately
/// separate so launcher diagnostics can never become tool output.
pub(crate) struct SandboxedCommand {
    launcher: Command,
    stdout: File,
    stderr: File,
}

impl SandboxedCommand {
    pub(crate) fn into_parts(self) -> (Command, File, File) {
        (self.launcher, self.stdout, self.stderr)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SystemRuntimeMount {
    source: PathBuf,
    target: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AuthorityMount {
    root: PathBuf,
    mount_id: u64,
    mount_point: PathBuf,
    device: u64,
    inode: u64,
    #[cfg(target_os = "macos")]
    filesystem_id: [u8; std::mem::size_of::<libc::fsid_t>()],
}

/// A protected workspace subtree selected before an agent command starts.
/// Its canonical path and real directory/mount identity remain frozen through
/// launch and post-command observation; later discovery cannot redefine the
/// set that Bubblewrap mounts read-only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FrozenReadOnlySurface {
    identity: AuthorityMount,
}

impl FrozenReadOnlySurface {
    pub(crate) fn path(&self) -> &Path {
        &self.identity.root
    }
}

/// Host path authority for structured tools, deliberately independent of
/// Bubblewrap. A missing process sandbox blocks subprocesses, but must neither
/// disable ordinary structured file tools nor let them cross a nested mount.
#[derive(Clone, Debug)]
pub(crate) struct StructuredPathAuthority {
    writable_roots: Vec<PathBuf>,
    authority_mounts: Result<Vec<AuthorityMount>, SandboxUnavailableReason>,
}

impl StructuredPathAuthority {
    pub(crate) fn new(writable_roots: Vec<PathBuf>) -> Self {
        let authority_mounts = inspect_structured_authority_roots(&writable_roots);
        Self {
            writable_roots,
            authority_mounts,
        }
    }

    /// Revalidate immediately before an agent-controlled path is opened. On
    /// Linux, mount id (not merely device id) catches same-device bind mounts;
    /// root inode comparison also catches path substitution after launch.
    pub(crate) fn validate(&self) -> Result<(), SandboxUnavailableReason> {
        let current_roots = self
            .writable_roots
            .iter()
            .map(|root| canonical_workspace(root))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
        if current_roots != self.writable_roots {
            return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
        }
        let expected = self.authority_mounts.as_ref().map_err(|reason| *reason)?;
        let current = inspect_structured_authority_roots(&self.writable_roots)?;
        if current != *expected {
            return Err(SandboxUnavailableReason::UnsafeMountTopology);
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn validate_opened_root(
        &self,
        root: &Path,
        descriptor: libc::c_int,
    ) -> Result<(), SandboxUnavailableReason> {
        let expected = self.expected_mount(root)?;
        let (device, inode) = macos_fd_file_identity(descriptor)?;
        if (device, inode) != (expected.device, expected.inode) {
            return Err(SandboxUnavailableReason::UnsafeMountTopology);
        }
        validate_macos_fd_mount(expected, descriptor)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn validate_opened_candidate(
        &self,
        root: &Path,
        descriptor: libc::c_int,
    ) -> Result<(), SandboxUnavailableReason> {
        validate_macos_fd_mount(self.expected_mount(root)?, descriptor)
    }

    #[cfg(target_os = "macos")]
    fn expected_mount(&self, root: &Path) -> Result<&AuthorityMount, SandboxUnavailableReason> {
        self.authority_mounts
            .as_ref()
            .map_err(|reason| *reason)?
            .iter()
            .find(|mount| mount.root == root)
            .ok_or(SandboxUnavailableReason::AuthorityInspectionFailed)
    }
}

impl WorkspaceSandbox {
    /// Build a profile for one workspace and inspect its host-backed authority.
    /// The complete Bubblewrap probe runs lazily on first use. Construction and
    /// probing never fall back to host execution: callers must inspect or
    /// propagate the resulting [`SandboxAvailability`].
    pub(crate) fn new(
        workspace: impl AsRef<Path>,
        attached_writable_roots: &[PathBuf],
        requested_runtime_roots: &[PathBuf],
        profile: SandboxProfile,
    ) -> Self {
        if !cfg!(target_os = "linux") {
            return Self::unavailable(
                profile,
                Vec::new(),
                Vec::new(),
                SandboxUnavailableReason::UnsupportedPlatform,
            );
        }
        let Ok(writable_roots) =
            canonical_writable_roots(workspace.as_ref(), attached_writable_roots)
        else {
            let reason = if canonical_workspace(workspace.as_ref()).is_ok() {
                SandboxUnavailableReason::InvalidWritableRoots
            } else {
                SandboxUnavailableReason::InvalidWorkspace
            };
            return Self::unavailable(profile, Vec::new(), Vec::new(), reason);
        };
        let Ok(runtime_roots) = canonical_runtime_roots(&writable_roots, requested_runtime_roots)
        else {
            return Self::unavailable(
                profile,
                writable_roots,
                Vec::new(),
                SandboxUnavailableReason::InvalidRuntimeRoots,
            );
        };
        if let Err(reason) = inspect_requested_root_resolution(
            workspace.as_ref(),
            attached_writable_roots,
            requested_runtime_roots,
        ) {
            return Self::unavailable(profile, writable_roots, runtime_roots, reason);
        }
        let Ok(system_runtime_mounts) = canonical_system_runtime_mounts() else {
            return Self::unavailable(
                profile,
                writable_roots,
                runtime_roots,
                SandboxUnavailableReason::AuthorityInspectionFailed,
            );
        };
        let authority_mounts =
            match freeze_authority_roots(&writable_roots, &runtime_roots, &system_runtime_mounts) {
                Ok(mounts) => mounts,
                Err(reason) => {
                    return Self::unavailable(profile, writable_roots, runtime_roots, reason);
                }
            };
        let Some(bwrap) = bwrap_path() else {
            return Self::unavailable(
                profile,
                writable_roots,
                runtime_roots,
                SandboxUnavailableReason::BubblewrapMissing,
            );
        };
        Self {
            writable_roots,
            runtime_roots,
            system_runtime_mounts,
            authority_mounts,
            bwrap: Some(bwrap),
            profile,
            availability: std::sync::OnceLock::new(),
        }
    }

    fn unavailable(
        profile: SandboxProfile,
        writable_roots: Vec<PathBuf>,
        runtime_roots: Vec<PathBuf>,
        reason: SandboxUnavailableReason,
    ) -> Self {
        Self {
            writable_roots,
            runtime_roots,
            system_runtime_mounts: Vec::new(),
            authority_mounts: Vec::new(),
            bwrap: None,
            profile,
            availability: std::sync::OnceLock::from(SandboxAvailability::Unavailable(reason)),
        }
    }

    pub(crate) fn availability(&self) -> SandboxAvailability {
        *self
            .availability
            .get_or_init(|| self.probe_current_profile())
    }

    /// Return a previously established result without starting the potentially
    /// expensive complete authority probe. Construction-time failures are
    /// already cached; a valid profile remains unknown until first use.
    pub(crate) fn cached_availability(&self) -> Option<SandboxAvailability> {
        self.availability.get().copied()
    }

    fn probe_current_profile(&self) -> SandboxAvailability {
        let read_only_surfaces =
            match freeze_read_only_workspace_surfaces_for_roots(&self.writable_roots) {
                Ok(surfaces) => surfaces,
                Err(reason) => return SandboxAvailability::Unavailable(reason),
            };
        self.probe_current_profile_with_surfaces(&read_only_surfaces)
    }

    fn probe_current_profile_with_surfaces(
        &self,
        read_only_surfaces: &[FrozenReadOnlySurface],
    ) -> SandboxAvailability {
        let current_mounts = match inspect_authority_roots(
            &self.writable_roots,
            &self.runtime_roots,
            &self.system_runtime_mounts,
        ) {
            Ok(mounts) => mounts,
            Err(reason) => return SandboxAvailability::Unavailable(reason),
        };
        if current_mounts != self.authority_mounts {
            return SandboxAvailability::Unavailable(SandboxUnavailableReason::UnsafeMountTopology);
        }
        if let Err(reason) =
            validate_frozen_read_only_surfaces(&self.writable_roots, read_only_surfaces)
        {
            return SandboxAvailability::Unavailable(reason);
        }
        let Some(bwrap) = self.bwrap.as_deref() else {
            return SandboxAvailability::Unavailable(SandboxUnavailableReason::CannotEnforce);
        };
        probe_profile(
            bwrap,
            &self.writable_roots,
            &self.runtime_roots,
            &self.system_runtime_mounts,
            read_only_surfaces,
            self.profile,
        )
    }

    pub(crate) const fn profile(&self) -> SandboxProfile {
        self.profile
    }

    pub(crate) fn writable_roots(&self) -> &[PathBuf] {
        &self.writable_roots
    }

    /// Every host-backed read-only source selected by the profile. These are
    /// provenance data, not a promise that a later launch cannot fail its
    /// final authority inspection.
    pub(crate) fn read_only_mounts(&self) -> Vec<PathBuf> {
        read_only_mount_sources(&self.runtime_roots, &self.system_runtime_mounts)
    }

    /// Wrap one program invocation in the enforced profile. Agent output is
    /// returned on dedicated pipes; the caller must keep launcher stdout and
    /// stderr private. An unavailable profile returns its concise public
    /// reason and never gives the caller an unsandboxed command.
    pub(crate) fn command<I, S>(
        &self,
        read_only_surfaces: &[FrozenReadOnlySurface],
        program: impl AsRef<OsStr>,
        args: I,
    ) -> Result<SandboxedCommand, SandboxUnavailableReason>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let availability = *self
            .availability
            .get_or_init(|| self.probe_current_profile_with_surfaces(read_only_surfaces));
        let SandboxAvailability::Enforced(_) = availability else {
            let SandboxAvailability::Unavailable(reason) = availability else {
                unreachable!("availability is either enforced or unavailable");
            };
            return Err(reason);
        };
        if self.writable_roots.is_empty() {
            return Err(SandboxUnavailableReason::InvalidWorkspace);
        }
        let current_mounts = inspect_authority_roots(
            &self.writable_roots,
            &self.runtime_roots,
            &self.system_runtime_mounts,
        )?;
        if current_mounts != self.authority_mounts {
            return Err(SandboxUnavailableReason::UnsafeMountTopology);
        }
        validate_frozen_read_only_surfaces(&self.writable_roots, read_only_surfaces)?;
        let bwrap = self
            .bwrap
            .as_deref()
            .ok_or(SandboxUnavailableReason::CannotEnforce)?;
        bwrap_command(
            bwrap,
            SandboxMountPlan {
                writable_roots: &self.writable_roots,
                runtime_roots: &self.runtime_roots,
                system_runtime_mounts: &self.system_runtime_mounts,
                read_only_surfaces,
            },
            program.as_ref(),
            args,
        )
    }
}

const BWRAP_PATHS: &[&str] = &["/usr/bin/bwrap", "/bin/bwrap"];
const SANDBOX_HOME: &str = "/tmp/home";
const SANDBOX_CACHE: &str = "/tmp/cache";
const PRIVATE_SANDBOX_MOUNT_TARGETS: &[&str] =
    &["/tmp", "/proc", "/dev", SANDBOX_HOME, SANDBOX_CACHE];
const RUNTIME_MOUNTS: &[&str] = &[
    "/usr/bin",
    "/usr/include",
    "/usr/lib",
    "/usr/lib64",
    "/usr/libexec",
    "/bin",
    "/lib",
    "/lib64",
];
/// Repository-local checkout collections can contain many independent roots.
/// They are readable substrate, not part of the primary root's implicit write
/// authority. A user must launch in a nested checkout to mutate it; only a
/// non-overlapping checkout can instead be attached as another root.
const READ_ONLY_WORKSPACE_SURFACES: &[&str] = &[".worktrees"];
const SYSTEM_SANDBOX_PATH: &str = "/usr/bin:/bin";
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Keep host-backed authority inspection bounded. An incomplete inspection
/// blocks subprocess launch rather than silently weakening the boundary.
#[cfg(target_os = "linux")]
const MAX_AUTHORITY_SCAN_ENTRIES: usize = 1_000_000;
pub const MAX_WRITABLE_ROOTS: usize = 8;
pub const MAX_RUNTIME_ROOTS: usize = 8;
const SANDBOX_STDOUT_READY_MARKER: &str = "\0__EULER_SANDBOX_STDOUT_READY__\0";
const SANDBOX_STDERR_READY_MARKER: &str = "\0__EULER_SANDBOX_STDERR_READY__\0";
// POSIX shell redirection numbers are only portable for single-digit file
// descriptors, while a live TUI can make these fresh pipe writers 10 or
// higher. Opening their private `/proc/self/fd` paths keeps arbitrary writer
// numbers valid under dash without sharing Bubblewrap's own stdout or stderr.
const SANDBOX_READY_WRAPPER: &str = concat!(
    "stdout_path=/proc/self/fd/$1; stderr_path=/proc/self/fd/$2; shift 2; ",
    "printf '\\000__EULER_SANDBOX_STDOUT_READY__\\000' >\"$stdout_path\" || exit 125; ",
    "printf '\\000__EULER_SANDBOX_STDERR_READY__\\000' >\"$stderr_path\" || exit 125; ",
    "exec \"$@\" >\"$stdout_path\" 2>\"$stderr_path\"",
);
#[cfg(target_os = "linux")]
const FIRST_INHERITED_FD: libc::c_uint = 3;
/// Keep the two agent-only writers out of both stdio and the single-digit
/// range. This exercises the same portable `/proc/self/fd` wrapper path in
/// every launch instead of letting descriptor pressure make it incidental.
#[cfg(target_os = "linux")]
const FIRST_AGENT_OUTPUT_FD: libc::c_int = 10;
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
#[cfg(target_os = "linux")]
const PROC_MOUNTINFO: &str = "/proc/self/mountinfo";
#[cfg(target_os = "linux")]
const RESOLVE_NO_XDEV: u64 = 0x01;
#[cfg(target_os = "linux")]
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
#[cfg(target_os = "linux")]
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
#[cfg(target_os = "linux")]
const RESOLVE_BENEATH: u64 = 0x08;
#[cfg(target_os = "linux")]
const ORDINARY_FILESYSTEM_TYPES: &[&str] = &[
    "bcachefs", "btrfs", "ecryptfs", "erofs", "ext2", "ext3", "ext4", "f2fs", "jfs", "nilfs2",
    "ntfs", "ntfs3", "overlay", "ramfs", "reiserfs", "rootfs", "squashfs", "tmpfs", "ubifs",
    "vfat", "xfs", "zfs",
];

#[cfg(target_os = "linux")]
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// Probe whether the network-isolated profile is enforceable for `workspace`.
///
/// The child process gets a private root, can access its workspace, cannot see
/// `/home`, and must enter a network namespace. A failure is intentionally
/// collapsed to a stable public reason: raw Bubblewrap diagnostics may expose
/// host details and are not suitable for model-facing or transcript output.
pub fn probe_workspace_sandbox(workspace: &Path) -> SandboxAvailability {
    WorkspaceSandbox::new(workspace, &[], &[], SandboxProfile::WorkspaceNoNetwork).availability()
}

/// Probe the complete multi-root profile that an agent shell would receive.
/// Every path is canonicalized and mounted writable only when the whole set is
/// distinct, non-overlapping, and enforceable.
pub fn probe_workspace_sandbox_with_roots(
    workspace: &Path,
    attached_writable_roots: &[PathBuf],
) -> SandboxAvailability {
    WorkspaceSandbox::new(
        workspace,
        attached_writable_roots,
        &[],
        SandboxProfile::WorkspaceNoNetwork,
    )
    .availability()
}

/// Validate that each dedicated output stream crossed the inner-wrapper
/// readiness boundary, then remove the frame. Bubblewrap's own stdio is a
/// separate private channel, so no launcher byte is accepted here.
pub(crate) fn strip_sandbox_stdout_ready_marker(
    stdout: &str,
) -> Result<&str, SandboxUnavailableReason> {
    strip_sandbox_stream_ready_marker(stdout, SANDBOX_STDOUT_READY_MARKER)
}

pub(crate) fn strip_sandbox_stderr_ready_marker(
    stderr: &str,
) -> Result<&str, SandboxUnavailableReason> {
    strip_sandbox_stream_ready_marker(stderr, SANDBOX_STDERR_READY_MARKER)
}

fn strip_sandbox_stream_ready_marker<'a>(
    stream: &'a str,
    marker: &str,
) -> Result<&'a str, SandboxUnavailableReason> {
    stream
        .split_once(marker)
        .map(|(_, child_output)| child_output)
        .ok_or(SandboxUnavailableReason::CannotEnforce)
}

fn probe_profile(
    bwrap: &Path,
    writable_roots: &[PathBuf],
    runtime_roots: &[PathBuf],
    system_runtime_mounts: &[SystemRuntimeMount],
    read_only_surfaces: &[FrozenReadOnlySurface],
    profile: SandboxProfile,
) -> SandboxAvailability {
    if writable_roots.is_empty() {
        return SandboxAvailability::Unavailable(SandboxUnavailableReason::InvalidWorkspace);
    }
    let mut probe_args = vec![
        std::ffi::OsString::from("-c"),
        std::ffi::OsString::from(
            "test -d /usr || exit 1; for root do test -w \"$root\" || exit 1; done",
        ),
        std::ffi::OsString::from("euler-sandbox-probe"),
    ];
    probe_args.extend(
        writable_roots
            .iter()
            .map(|root| root.as_os_str().to_os_string()),
    );
    let Ok(sandboxed) = bwrap_command(
        bwrap,
        SandboxMountPlan {
            writable_roots,
            runtime_roots,
            system_runtime_mounts,
            read_only_surfaces,
        },
        OsStr::new("/bin/sh"),
        probe_args,
    ) else {
        return SandboxAvailability::Unavailable(SandboxUnavailableReason::CannotEnforce);
    };
    let (mut command, _agent_stdout, _agent_stderr) = sandboxed.into_parts();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = command.spawn() else {
        return SandboxAvailability::Unavailable(SandboxUnavailableReason::CannotEnforce);
    };
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                return SandboxAvailability::Enforced(profile);
            }
            Ok(Some(_)) | Err(_) => {
                return SandboxAvailability::Unavailable(SandboxUnavailableReason::CannotEnforce);
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return SandboxAvailability::Unavailable(SandboxUnavailableReason::CannotEnforce);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
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

fn canonical_system_runtime_mounts() -> Result<Vec<SystemRuntimeMount>, SandboxUnavailableReason> {
    RUNTIME_MOUNTS
        .iter()
        .map(|target| {
            let target = PathBuf::from(target);
            if !target.exists() {
                return Ok(None);
            }
            let source = target
                .canonicalize()
                .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
            if !source.is_dir() {
                return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
            }
            Ok(Some(SystemRuntimeMount { source, target }))
        })
        .filter_map(Result::transpose)
        .collect()
}

fn read_only_mount_sources(
    runtime_roots: &[PathBuf],
    system_runtime_mounts: &[SystemRuntimeMount],
) -> Vec<PathBuf> {
    system_runtime_mounts
        .iter()
        .map(|mount| mount.source.clone())
        .chain(runtime_roots.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Reject Linux magic-link traversal before canonicalization can erase the
/// fact that a requested root reached its target through procfs. Ordinary
/// symlinks remain valid root aliases; the resolved mount topology is checked
/// separately below.
#[cfg(target_os = "linux")]
fn inspect_requested_root_resolution(
    workspace: &Path,
    attached_writable_roots: &[PathBuf],
    runtime_roots: &[PathBuf],
) -> Result<(), SandboxUnavailableReason> {
    for root in std::iter::once(workspace)
        .chain(attached_writable_roots.iter().map(PathBuf::as_path))
        .chain(runtime_roots.iter().map(PathBuf::as_path))
    {
        open_without_magic_links(root)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_without_magic_links(path: &Path) -> Result<(), SandboxUnavailableReason> {
    use std::os::unix::ffi::OsStrExt as _;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
    let flags = u64::try_from(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
    let how = OpenHow {
        flags,
        mode: 0,
        resolve: RESOLVE_NO_MAGICLINKS,
    };
    // SAFETY: `path` is NUL-terminated and `how` is the kernel's three-u64
    // `open_how` layout for the supplied size. No descriptor is inherited.
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if descriptor < 0 {
        return if std::io::Error::last_os_error().raw_os_error() == Some(libc::ELOOP) {
            Err(SandboxUnavailableReason::UnsafeMountTopology)
        } else {
            Err(SandboxUnavailableReason::AuthorityInspectionFailed)
        };
    }
    let descriptor = libc::c_int::try_from(descriptor)
        .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
    // SAFETY: `descriptor` was returned by `openat2` above and is owned here.
    if unsafe { libc::close(descriptor) } != 0 {
        return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
    }
    Ok(())
}

/// Open a structured-tool file relative to an already-authorized root without
/// crossing a mount or following a raced symlink. `RESOLVE_NO_XDEV` uses mount
/// identity, so a same-device bind mount is rejected as firmly as a different
/// filesystem. Callers still validate the root set before invoking this final
/// race-resistant open.
#[cfg(target_os = "linux")]
pub(crate) fn open_structured_file_beneath(
    root: &Path,
    relative: &Path,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> std::io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Component;

    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "structured path must be normalized and relative",
        ));
    }
    let root = std::ffi::CString::new(root.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let relative = std::ffi::CString::new(relative.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: `root` is NUL-terminated and the returned descriptor is either
    // negative or uniquely owned and immediately wrapped in `File`.
    let root_fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if root_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `open` returned a fresh descriptor owned by this function.
    let root_file = unsafe { std::fs::File::from_raw_fd(root_fd) };
    let flags = u64::try_from(flags | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let how = OpenHow {
        flags,
        mode: u64::from(mode),
        resolve: RESOLVE_BENEATH | RESOLVE_NO_XDEV | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS,
    };
    // SAFETY: both path strings are NUL-terminated, `root_file` remains live,
    // and `how` is the kernel's three-u64 `open_how` layout for this size.
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root_file.as_raw_fd(),
            relative.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let descriptor = libc::c_int::try_from(descriptor)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
    // SAFETY: `openat2` returned a fresh descriptor owned by the result.
    Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
}

/// Freeze the directory and mount identity of every host-backed bind source.
///
/// The content walk is deliberately separate: it is required before the
/// profile is reported as available and again at the final launch boundary,
/// but a complete walk here would make every ordinary Euler startup
/// proportional to the workspace and host runtime even when no subprocess is
/// requested.
#[cfg(target_os = "linux")]
fn freeze_authority_roots(
    writable_roots: &[PathBuf],
    runtime_roots: &[PathBuf],
    system_runtime_mounts: &[SystemRuntimeMount],
) -> Result<Vec<AuthorityMount>, SandboxUnavailableReason> {
    use std::os::unix::fs::MetadataExt as _;

    let mount_table = MountTable::read()?;
    let roots = writable_roots
        .iter()
        .cloned()
        .chain(read_only_mount_sources(
            runtime_roots,
            system_runtime_mounts,
        ))
        .collect::<std::collections::BTreeSet<_>>();
    let mut roots = roots.into_iter().collect::<Vec<_>>();
    roots.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    let mut authority_mounts = Vec::with_capacity(roots.len());
    for root in roots {
        let canonical = root
            .canonicalize()
            .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
        let metadata = std::fs::symlink_metadata(&root)
            .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
        if canonical != root || !metadata.file_type().is_dir() {
            return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
        }
        let (mount_id, mount_point) = mount_table.validate_root(&canonical)?;
        authority_mounts.push(AuthorityMount {
            root,
            mount_id,
            mount_point,
            device: metadata.dev(),
            inode: metadata.ino(),
        });
    }
    Ok(authority_mounts)
}

/// Inspect every host-backed bind source without following symlinks.
/// Unix sockets, FIFOs, and device nodes carry authority independently of
/// normal filesystem permissions, so exposing even a read-only node would
/// weaken the workspace boundary. A bounded or incomplete walk fails closed.
#[cfg(target_os = "linux")]
fn inspect_authority_roots(
    writable_roots: &[PathBuf],
    runtime_roots: &[PathBuf],
    system_runtime_mounts: &[SystemRuntimeMount],
) -> Result<Vec<AuthorityMount>, SandboxUnavailableReason> {
    let authority_mounts =
        freeze_authority_roots(writable_roots, runtime_roots, system_runtime_mounts)?;
    let mut roots = writable_roots
        .iter()
        .cloned()
        .chain(read_only_mount_sources(
            runtime_roots,
            system_runtime_mounts,
        ))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    roots.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    let mut inspected = 0_usize;
    let mut inspected_roots = Vec::<PathBuf>::new();
    for root in roots {
        if inspected_roots
            .iter()
            .any(|inspected_root| root.starts_with(inspected_root))
        {
            continue;
        }
        inspect_authority_root(&root, &mut inspected)?;
        inspected_roots.push(root);
    }
    Ok(authority_mounts)
}

/// Inspect only the writable path boundary used by structured tools. Unlike
/// subprocess inspection this does not depend on system runtime mounts,
/// Bubblewrap, or a platform process-sandbox implementation.
#[cfg(target_os = "linux")]
fn inspect_structured_authority_roots(
    writable_roots: &[PathBuf],
) -> Result<Vec<AuthorityMount>, SandboxUnavailableReason> {
    use std::os::unix::fs::MetadataExt as _;

    if writable_roots.is_empty() {
        return Err(SandboxUnavailableReason::InvalidWorkspace);
    }
    let mount_table = MountTable::read()?;
    writable_roots
        .iter()
        .map(|root| {
            let canonical = root
                .canonicalize()
                .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
            let metadata = std::fs::symlink_metadata(root)
                .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
            if canonical != *root || !metadata.file_type().is_dir() {
                return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
            }
            let (mount_id, mount_point) = mount_table.validate_root(&canonical)?;
            Ok(AuthorityMount {
                root: root.clone(),
                mount_id,
                mount_point,
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn inspect_structured_authority_roots(
    writable_roots: &[PathBuf],
) -> Result<Vec<AuthorityMount>, SandboxUnavailableReason> {
    use std::os::unix::fs::MetadataExt as _;

    if writable_roots.is_empty() {
        return Err(SandboxUnavailableReason::InvalidWorkspace);
    }
    let mounts = macos_mount_table()?;
    writable_roots
        .iter()
        .map(|root| {
            let canonical = root
                .canonicalize()
                .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
            let metadata = std::fs::symlink_metadata(root)
                .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
            if canonical != *root || !metadata.file_type().is_dir() {
                return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
            }
            let descriptor = open_macos_directory(root)?;
            let descriptor_identity = macos_fd_file_identity({
                use std::os::fd::AsRawFd as _;
                descriptor.as_raw_fd()
            })?;
            if descriptor_identity != (metadata.dev(), metadata.ino()) {
                return Err(SandboxUnavailableReason::UnsafeMountTopology);
            }
            let selected = macos_fd_mount_identity({
                use std::os::fd::AsRawFd as _;
                descriptor.as_raw_fd()
            })?;
            let matching_mounts = mounts.iter().filter(|mount| **mount == selected).count();
            if matching_mounts != 1
                || mounts.iter().any(|mount| {
                    mount.mount_point.as_path() != root.as_path()
                        && mount.mount_point.starts_with(root)
                })
            {
                return Err(SandboxUnavailableReason::UnsafeMountTopology);
            }
            Ok(AuthorityMount {
                root: root.clone(),
                mount_id: 0,
                mount_point: selected.mount_point.clone(),
                device: metadata.dev(),
                inode: metadata.ino(),
                filesystem_id: selected.filesystem_id,
            })
        })
        .collect()
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, Eq, PartialEq)]
struct MacosMountEntry {
    mount_point: PathBuf,
    filesystem_id: [u8; std::mem::size_of::<libc::fsid_t>()],
}

#[cfg(target_os = "macos")]
fn macos_mount_table() -> Result<Vec<MacosMountEntry>, SandboxUnavailableReason> {
    use std::os::unix::ffi::OsStringExt as _;

    let entries = macos_owned_mount_entries()?;
    let mut mounts = Vec::with_capacity(entries.len());
    for entry in entries {
        let bytes = macos_fixed_array_bytes(&entry.f_mntonname)?;
        let path = PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()));
        if !is_normalized_absolute_mount_path(&path) {
            return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
        }
        mounts.push(MacosMountEntry {
            mount_point: path,
            filesystem_id: macos_fsid_bytes(&entry.f_fsid),
        });
    }
    mounts.sort_by(|left, right| left.mount_point.cmp(&right.mount_point));
    if mounts
        .windows(2)
        .any(|pair| pair[0].mount_point == pair[1].mount_point)
    {
        return Err(SandboxUnavailableReason::UnsafeMountTopology);
    }
    if mounts.is_empty() {
        return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
    }
    Ok(mounts)
}

#[cfg(target_os = "macos")]
fn open_macos_directory(path: &Path) -> Result<std::fs::File, SandboxUnavailableReason> {
    use std::os::fd::FromRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
    // SAFETY: `path` is NUL-terminated and a successful fresh descriptor is
    // immediately transferred into File ownership.
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if descriptor < 0 {
        return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
    }
    // SAFETY: `open` returned a fresh descriptor owned here.
    Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
}

#[cfg(target_os = "macos")]
fn macos_owned_mount_entries() -> Result<Vec<libc::statfs>, SandboxUnavailableReason> {
    const MAX_MOUNTS: usize = 16_384;
    const MAX_ATTEMPTS: usize = 4;

    // SAFETY: a null buffer and zero length asks Darwin only for the current
    // entry count; Euler never borrows libc-owned mount storage.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    let count = usize::try_from(count)
        .ok()
        .filter(|count| *count > 0 && *count < MAX_MOUNTS)
        .ok_or(SandboxUnavailableReason::AuthorityInspectionFailed)?;
    let mut capacity = count.saturating_add(16).min(MAX_MOUNTS);
    for _ in 0..MAX_ATTEMPTS {
        let mut storage = Vec::<std::mem::MaybeUninit<libc::statfs>>::new();
        storage.resize_with(capacity, std::mem::MaybeUninit::uninit);
        let byte_len = capacity
            .checked_mul(std::mem::size_of::<libc::statfs>())
            .and_then(|bytes| libc::c_int::try_from(bytes).ok())
            .ok_or(SandboxUnavailableReason::AuthorityInspectionFailed)?;
        // SAFETY: `storage` is an owned, correctly aligned buffer of
        // `capacity` statfs slots and `byte_len` is its exact initialized
        // extent. Darwin writes at most that many bytes.
        let returned =
            unsafe { libc::getfsstat(storage.as_mut_ptr().cast(), byte_len, libc::MNT_NOWAIT) };
        let returned = usize::try_from(returned)
            .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
        if returned < capacity {
            // SAFETY: a successful `getfsstat` initialized exactly the first
            // `returned` statfs entries in Euler's owned buffer.
            return Ok(storage
                .into_iter()
                .take(returned)
                .map(|entry| unsafe { entry.assume_init() })
                .collect());
        }
        capacity = capacity
            .checked_mul(2)
            .filter(|capacity| *capacity <= MAX_MOUNTS)
            .ok_or(SandboxUnavailableReason::AuthorityInspectionFailed)?;
    }
    Err(SandboxUnavailableReason::AuthorityInspectionFailed)
}

#[cfg(target_os = "macos")]
fn macos_fixed_array_bytes<const N: usize>(
    value: &[libc::c_char; N],
) -> Result<Vec<u8>, SandboxUnavailableReason> {
    let end = value
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(SandboxUnavailableReason::AuthorityInspectionFailed)?;
    Ok(value[..end]
        .iter()
        .map(|byte| u8::from_ne_bytes(byte.to_ne_bytes()))
        .collect())
}

#[cfg(target_os = "macos")]
fn macos_fsid_bytes(filesystem_id: &libc::fsid_t) -> [u8; std::mem::size_of::<libc::fsid_t>()] {
    let mut bytes = [0; std::mem::size_of::<libc::fsid_t>()];
    // SAFETY: `filesystem_id` is initialized kernel output and `bytes` has
    // exactly the same size. Reading its object representation as bytes is
    // valid and does not depend on libc's private field names.
    unsafe {
        std::ptr::copy_nonoverlapping(
            std::ptr::from_ref(filesystem_id).cast::<u8>(),
            bytes.as_mut_ptr(),
            bytes.len(),
        );
    }
    bytes
}

#[cfg(target_os = "macos")]
fn macos_fd_file_identity(descriptor: libc::c_int) -> Result<(u64, u64), SandboxUnavailableReason> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `metadata` is writable storage for one stat and is read only
    // after `fstat` reports success.
    if unsafe { libc::fstat(descriptor, metadata.as_mut_ptr()) } != 0 {
        return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
    }
    // SAFETY: initialized by successful fstat.
    let metadata = unsafe { metadata.assume_init() };
    let device = u64::try_from(metadata.st_dev)
        .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
    Ok((device, metadata.st_ino))
}

#[cfg(target_os = "macos")]
fn macos_fd_mount_identity(
    descriptor: libc::c_int,
) -> Result<MacosMountEntry, SandboxUnavailableReason> {
    use std::os::unix::ffi::OsStringExt as _;

    let mut metadata = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `metadata` is writable storage for one statfs and is read only
    // after `fstatfs` reports success.
    if unsafe { libc::fstatfs(descriptor, metadata.as_mut_ptr()) } != 0 {
        return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
    }
    // SAFETY: initialized by successful fstatfs.
    let metadata = unsafe { metadata.assume_init() };
    let mount_point = PathBuf::from(std::ffi::OsString::from_vec(macos_fixed_array_bytes(
        &metadata.f_mntonname,
    )?));
    if !is_normalized_absolute_mount_path(&mount_point) {
        return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
    }
    Ok(MacosMountEntry {
        mount_point,
        filesystem_id: macos_fsid_bytes(&metadata.f_fsid),
    })
}

#[cfg(target_os = "macos")]
fn validate_macos_fd_mount(
    expected: &AuthorityMount,
    descriptor: libc::c_int,
) -> Result<(), SandboxUnavailableReason> {
    let observed = macos_fd_mount_identity(descriptor)?;
    if observed.mount_point == expected.mount_point
        && observed.filesystem_id == expected.filesystem_id
    {
        Ok(())
    } else {
        Err(SandboxUnavailableReason::UnsafeMountTopology)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn inspect_structured_authority_roots(
    _writable_roots: &[PathBuf],
) -> Result<Vec<AuthorityMount>, SandboxUnavailableReason> {
    Err(SandboxUnavailableReason::StructuredPathUnsupportedPlatform)
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct MountTable {
    entries: Vec<MountEntry>,
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct MountEntry {
    mount_id: u64,
    mount_point: PathBuf,
    filesystem_type: String,
}

#[cfg(target_os = "linux")]
impl MountEntry {
    fn has_valid_structure(&self) -> bool {
        self.mount_id != 0
            && is_normalized_absolute_mount_path(&self.mount_point)
            && !self.filesystem_type.is_empty()
    }
}

#[cfg(target_os = "linux")]
impl MountTable {
    fn read() -> Result<Self, SandboxUnavailableReason> {
        let contents = std::fs::read_to_string(PROC_MOUNTINFO)
            .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
        parse_mount_table(&contents)
    }

    fn validate_root(&self, root: &Path) -> Result<(u64, PathBuf), SandboxUnavailableReason> {
        if !is_normalized_absolute_mount_path(root) {
            return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
        }
        let containing = self
            .entries
            .iter()
            .filter(|entry| root.starts_with(&entry.mount_point))
            .collect::<Vec<_>>();
        let depth = containing
            .iter()
            .map(|entry| entry.mount_point.components().count())
            .max()
            .ok_or(SandboxUnavailableReason::AuthorityInspectionFailed)?;
        let selected = containing
            .iter()
            .filter(|entry| entry.mount_point.components().count() == depth)
            .copied()
            .collect::<Vec<_>>();
        if selected.len() != 1
            || !ORDINARY_FILESYSTEM_TYPES.contains(&selected[0].filesystem_type.as_str())
            || self.entries.iter().any(|entry| {
                entry.mount_id != selected[0].mount_id
                    && entry.mount_point != root
                    && entry.mount_point.starts_with(root)
            })
        {
            return Err(SandboxUnavailableReason::UnsafeMountTopology);
        }
        Ok((selected[0].mount_id, selected[0].mount_point.clone()))
    }
}

#[cfg(target_os = "linux")]
fn parse_mount_table(contents: &str) -> Result<MountTable, SandboxUnavailableReason> {
    let entries = contents
        .lines()
        .map(parse_mount_entry)
        .collect::<Result<Vec<_>, _>>()?;
    let mut mount_ids = std::collections::HashSet::with_capacity(entries.len());
    if entries.is_empty()
        || entries
            .iter()
            .any(|entry| !entry.has_valid_structure() || !mount_ids.insert(entry.mount_id))
    {
        return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
    }
    Ok(MountTable { entries })
}

#[cfg(target_os = "linux")]
fn parse_mount_entry(line: &str) -> Result<MountEntry, SandboxUnavailableReason> {
    let fields = line.split_ascii_whitespace().collect::<Vec<_>>();
    let separator = fields
        .iter()
        .position(|field| *field == "-")
        .ok_or(SandboxUnavailableReason::AuthorityInspectionFailed)?;
    if separator < 6 || fields.get(separator + 3).is_none() {
        return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
    }
    let mount_root = fields
        .get(3)
        .ok_or(SandboxUnavailableReason::AuthorityInspectionFailed)?;
    if decode_mount_path(mount_root)?.as_os_str().is_empty() {
        return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
    }
    let mount_point = fields
        .get(4)
        .ok_or(SandboxUnavailableReason::AuthorityInspectionFailed)?;
    let filesystem_type = fields
        .get(separator + 1)
        .ok_or(SandboxUnavailableReason::AuthorityInspectionFailed)?;
    let mount_id = fields
        .first()
        .ok_or(SandboxUnavailableReason::AuthorityInspectionFailed)?
        .parse::<u64>()
        .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
    fields
        .get(1)
        .ok_or(SandboxUnavailableReason::AuthorityInspectionFailed)?
        .parse::<u64>()
        .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
    parse_mount_device(
        fields
            .get(2)
            .ok_or(SandboxUnavailableReason::AuthorityInspectionFailed)?,
    )?;
    Ok(MountEntry {
        mount_id,
        mount_point: decode_mount_path(mount_point)?,
        filesystem_type: (*filesystem_type).to_owned(),
    })
}

#[cfg(target_os = "linux")]
fn parse_mount_device(field: &str) -> Result<(), SandboxUnavailableReason> {
    let (major, minor) = field
        .split_once(':')
        .ok_or(SandboxUnavailableReason::AuthorityInspectionFailed)?;
    major
        .parse::<u32>()
        .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
    minor
        .parse::<u32>()
        .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_normalized_absolute_mount_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    let bytes = path.as_os_str().as_bytes();
    if bytes == b"/" {
        return true;
    }
    bytes.starts_with(b"/")
        && !bytes.ends_with(b"/")
        && !bytes.contains(&0)
        && bytes[1..]
            .split(|byte| *byte == b'/')
            .all(|component| !component.is_empty() && component != b"." && component != b"..")
}

#[cfg(target_os = "linux")]
fn decode_mount_path(field: &str) -> Result<PathBuf, SandboxUnavailableReason> {
    use std::os::unix::ffi::OsStringExt as _;

    let input = field.as_bytes();
    let mut decoded = Vec::with_capacity(input.len());
    let mut index = 0_usize;
    while index < input.len() {
        if input[index] != b'\\' {
            decoded.push(input[index]);
            index += 1;
            continue;
        }
        let digits = input
            .get(index + 1..index + 4)
            .ok_or(SandboxUnavailableReason::AuthorityInspectionFailed)?;
        if !digits.iter().all(u8::is_ascii_digit) || digits.iter().any(|digit| *digit > b'7') {
            return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
        }
        let value = (digits[0] - b'0') * 64 + (digits[1] - b'0') * 8 + (digits[2] - b'0');
        if !matches!(value, b' ' | b'\t' | b'\n' | b'\\') {
            return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
        }
        decoded.push(value);
        index += 4;
    }
    Ok(std::ffi::OsString::from_vec(decoded).into())
}

#[cfg(target_os = "linux")]
fn inspect_authority_root(
    root: &Path,
    inspected: &mut usize,
) -> Result<(), SandboxUnavailableReason> {
    use std::os::unix::fs::FileTypeExt;

    *inspected = inspected
        .checked_add(1)
        .filter(|count| *count <= MAX_AUTHORITY_SCAN_ENTRIES)
        .ok_or(SandboxUnavailableReason::AuthorityInspectionFailed)?;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
        let file_type = metadata.file_type();
        if file_type.is_socket()
            || file_type.is_fifo()
            || file_type.is_block_device()
            || file_type.is_char_device()
        {
            return Err(SandboxUnavailableReason::UnsafeSpecialNode);
        }
        if file_type.is_dir() {
            let entries = std::fs::read_dir(&path)
                .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
            for entry in entries {
                let entry =
                    entry.map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
                // `DirEntry::file_type` uses the directory entry's type when
                // the filesystem supplies it and falls back to `fstatat`
                // otherwise. It does not follow symlinks, so ordinary files
                // need no second pathname lookup while special nodes are
                // still rejected and every directory is walked.
                let entry_type = entry
                    .file_type()
                    .map_err(|_| SandboxUnavailableReason::AuthorityInspectionFailed)?;
                if entry_type.is_socket()
                    || entry_type.is_fifo()
                    || entry_type.is_block_device()
                    || entry_type.is_char_device()
                {
                    return Err(SandboxUnavailableReason::UnsafeSpecialNode);
                }
                if !entry_type.is_file() && !entry_type.is_symlink() && !entry_type.is_dir() {
                    return Err(SandboxUnavailableReason::UnsafeSpecialNode);
                }
                // Count discoveries before queueing them so a single broad
                // directory cannot allocate an unbounded pending list before
                // the scan notices its entry limit.
                *inspected = inspected
                    .checked_add(1)
                    .filter(|count| *count <= MAX_AUTHORITY_SCAN_ENTRIES)
                    .ok_or(SandboxUnavailableReason::AuthorityInspectionFailed)?;
                if entry_type.is_dir() {
                    pending.push(entry.path());
                }
            }
        } else if !file_type.is_file() && !file_type.is_symlink() {
            return Err(SandboxUnavailableReason::UnsafeSpecialNode);
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn inspect_requested_root_resolution(
    _workspace: &Path,
    _attached_writable_roots: &[PathBuf],
    _runtime_roots: &[PathBuf],
) -> Result<(), SandboxUnavailableReason> {
    Err(SandboxUnavailableReason::UnsupportedPlatform)
}

#[cfg(not(target_os = "linux"))]
fn inspect_authority_roots(
    _writable_roots: &[PathBuf],
    _runtime_roots: &[PathBuf],
    _system_runtime_mounts: &[SystemRuntimeMount],
) -> Result<Vec<AuthorityMount>, SandboxUnavailableReason> {
    Err(SandboxUnavailableReason::UnsupportedPlatform)
}

#[cfg(not(target_os = "linux"))]
fn freeze_authority_roots(
    _writable_roots: &[PathBuf],
    _runtime_roots: &[PathBuf],
    _system_runtime_mounts: &[SystemRuntimeMount],
) -> Result<Vec<AuthorityMount>, SandboxUnavailableReason> {
    Err(SandboxUnavailableReason::UnsupportedPlatform)
}

pub fn canonical_writable_roots(
    workspace: &Path,
    attached_writable_roots: &[PathBuf],
) -> Result<Vec<PathBuf>, std::io::Error> {
    canonical_writable_roots_from_primary(canonical_workspace(workspace)?, attached_writable_roots)
}

/// Canonicalize an explicit read-only runtime/toolchain set. Runtime roots
/// must be directories and may not overlap writable roots or each other: bind
/// order must never silently change a requested authority boundary.
pub fn canonical_runtime_roots(
    writable_roots: &[PathBuf],
    runtime_roots: &[PathBuf],
) -> Result<Vec<PathBuf>, std::io::Error> {
    if runtime_roots.is_empty() {
        return Ok(Vec::new());
    }
    let configured_home = configured_host_home()?;
    canonical_runtime_roots_for_home(writable_roots, runtime_roots, configured_home.as_deref())
}

fn canonical_runtime_roots_for_home(
    writable_roots: &[PathBuf],
    runtime_roots: &[PathBuf],
    configured_home: Option<&Path>,
) -> Result<Vec<PathBuf>, std::io::Error> {
    if runtime_roots.len() > MAX_RUNTIME_ROOTS {
        return Err(invalid_runtime_roots());
    }
    let roots = runtime_roots
        .iter()
        .map(|root| canonical_workspace(root))
        .collect::<Result<Vec<_>, _>>()?;
    if roots.iter().any(|root| {
        root == Path::new("/")
            || overrides_private_sandbox_mount(root)
            || root.to_str().is_none_or(|root| root.contains(':'))
            || configured_home.is_some_and(|home| home.starts_with(root))
    }) {
        return Err(invalid_runtime_roots());
    }
    for (index, root) in roots.iter().enumerate() {
        let runtime_overlap = roots
            .iter()
            .skip(index + 1)
            .any(|other| paths_overlap(root, other));
        if runtime_overlap
            || writable_roots
                .iter()
                .any(|other| paths_overlap(root, other))
        {
            return Err(invalid_runtime_roots());
        }
    }
    Ok(roots)
}

fn configured_host_home() -> Result<Option<PathBuf>, std::io::Error> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    home.map(|path| canonical_workspace(Path::new(&path)))
        .transpose()
        .map_err(|_| invalid_runtime_roots())
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn canonical_writable_roots_from_primary(
    workspace: PathBuf,
    attached_writable_roots: &[PathBuf],
) -> Result<Vec<PathBuf>, std::io::Error> {
    let requested = 1_usize
        .checked_add(attached_writable_roots.len())
        .ok_or_else(invalid_writable_roots)?;
    if requested > MAX_WRITABLE_ROOTS {
        return Err(invalid_writable_roots());
    }
    let mut roots = Vec::with_capacity(requested);
    roots.push(workspace);
    for root in attached_writable_roots {
        roots.push(canonical_workspace(root)?);
    }
    if roots.iter().any(|root| {
        root == Path::new("/") || overrides_private_sandbox_mount(root) || root.to_str().is_none()
    }) {
        return Err(invalid_writable_roots());
    }
    for (index, root) in roots.iter().enumerate() {
        if roots
            .iter()
            .skip(index + 1)
            .any(|other| root == other || root.starts_with(other) || other.starts_with(root))
        {
            return Err(invalid_writable_roots());
        }
    }
    Ok(roots)
}

fn overrides_private_sandbox_mount(root: &Path) -> bool {
    PRIVATE_SANDBOX_MOUNT_TARGETS
        .iter()
        .map(Path::new)
        .any(|target| target.starts_with(root))
}

fn invalid_writable_roots() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "writable roots must be distinct, non-overlapping directories that do not replace private sandbox mounts",
    )
}

fn invalid_runtime_roots() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "runtime roots must be distinct directories outside writable roots and must not contain the configured home or replace private sandbox mounts",
    )
}

struct SandboxMountPlan<'a> {
    writable_roots: &'a [PathBuf],
    runtime_roots: &'a [PathBuf],
    system_runtime_mounts: &'a [SystemRuntimeMount],
    read_only_surfaces: &'a [FrozenReadOnlySurface],
}

fn bwrap_command<I, S>(
    bwrap: &Path,
    mounts: SandboxMountPlan<'_>,
    program: &OsStr,
    args: I,
) -> Result<SandboxedCommand, SandboxUnavailableReason>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let workspace = mounts
        .writable_roots
        .first()
        .expect("a probed sandbox always has a primary workspace");
    let sandbox_path = sandbox_path(mounts.runtime_roots);
    let mut command = Command::new(bwrap);
    // Clear the launcher too: `--clearenv` protects the inner command, while
    // this prevents an inherited loader/configuration variable from changing
    // Bubblewrap before it establishes the namespace.
    command.env_clear();
    let output = prepare_sandbox_output(&mut command)?;
    configure_bwrap_profile(&mut command, &mounts, &sandbox_path);
    command
        .arg("--chdir")
        .arg(workspace)
        .args(["--", "/bin/sh", "-c"])
        .arg(SANDBOX_READY_WRAPPER)
        .arg("euler-sandbox")
        .arg(output.stdout_fd.to_string())
        .arg(output.stderr_fd.to_string())
        .arg(program)
        .args(args);
    Ok(output.finish(command))
}

fn configure_bwrap_profile(
    command: &mut Command,
    mounts: &SandboxMountPlan<'_>,
    sandbox_path: &str,
) {
    command.args([
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-net",
        "--disable-userns",
        "--cap-drop",
        "ALL",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
        "--setenv",
        "HOME",
        SANDBOX_HOME,
        "--setenv",
        "XDG_CACHE_HOME",
        SANDBOX_CACHE,
        "--setenv",
        "TMPDIR",
        "/tmp",
        "--setenv",
        "PATH",
        sandbox_path,
        "--tmpfs",
        "/",
    ]);
    for mount in mounts.system_runtime_mounts {
        add_system_runtime_mount(command, mount);
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
    for root in mounts.runtime_roots {
        add_mount_target_directories(command, root);
        command.arg("--ro-bind").arg(root).arg(root);
    }
    for root in mounts.writable_roots {
        add_mount_target_directories(command, root);
        command.arg("--bind").arg(root).arg(root);
    }
    for surface in mounts.read_only_surfaces {
        command
            .arg("--ro-bind")
            .arg(surface.path())
            .arg(surface.path());
    }
}

fn add_system_runtime_mount(command: &mut Command, mount: &SystemRuntimeMount) {
    add_mount_target_directories(command, &mount.target);
    command
        .arg("--ro-bind")
        .arg(&mount.source)
        .arg(&mount.target);
}

pub(crate) fn is_read_only_workspace_surface(relative: &Path) -> bool {
    relative
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .is_some_and(|name| READ_ONLY_WORKSPACE_SURFACES.contains(&name))
}

fn read_only_workspace_surfaces(root: &Path) -> Vec<PathBuf> {
    READ_ONLY_WORKSPACE_SURFACES
        .iter()
        .map(|name| root.join(name))
        .filter(|path| fs_metadata_is_directory_without_following_symlinks(path).unwrap_or(false))
        .collect()
}

pub(crate) fn freeze_read_only_workspace_surfaces(
    root: &Path,
) -> Result<Vec<FrozenReadOnlySurface>, SandboxUnavailableReason> {
    let paths = read_only_workspace_surfaces(root);
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    inspect_structured_authority_roots(&paths).map(|identities| {
        identities
            .into_iter()
            .map(|identity| FrozenReadOnlySurface { identity })
            .collect()
    })
}

fn freeze_read_only_workspace_surfaces_for_roots(
    writable_roots: &[PathBuf],
) -> Result<Vec<FrozenReadOnlySurface>, SandboxUnavailableReason> {
    writable_roots
        .iter()
        .map(|root| freeze_read_only_workspace_surfaces(root))
        .collect::<Result<Vec<_>, _>>()
        .map(|surfaces| surfaces.into_iter().flatten().collect())
}

pub(crate) fn validate_frozen_read_only_surfaces(
    writable_roots: &[PathBuf],
    surfaces: &[FrozenReadOnlySurface],
) -> Result<(), SandboxUnavailableReason> {
    let allowed = writable_roots
        .iter()
        .flat_map(|root| {
            READ_ONLY_WORKSPACE_SURFACES
                .iter()
                .map(move |name| root.join(name))
        })
        .collect::<std::collections::BTreeSet<_>>();
    let selected = surfaces
        .iter()
        .map(|surface| surface.path().to_path_buf())
        .collect::<std::collections::BTreeSet<_>>();
    if selected.len() != surfaces.len() || !selected.is_subset(&allowed) {
        return Err(SandboxUnavailableReason::AuthorityInspectionFailed);
    }
    if surfaces.is_empty() {
        return Ok(());
    }
    let current = inspect_structured_authority_roots(&selected.into_iter().collect::<Vec<_>>())?;
    if current.len() == surfaces.len()
        && current
            .iter()
            .all(|identity| surfaces.iter().any(|surface| surface.identity == *identity))
    {
        Ok(())
    } else {
        Err(SandboxUnavailableReason::UnsafeMountTopology)
    }
}

fn fs_metadata_is_directory_without_following_symlinks(path: &Path) -> std::io::Result<bool> {
    Ok(std::fs::symlink_metadata(path)?.file_type().is_dir())
}

fn sandbox_path(runtime_roots: &[PathBuf]) -> String {
    let mut entries = runtime_roots
        .iter()
        .flat_map(|root| {
            let bin = root.join("bin");
            [bin.is_dir().then_some(bin), Some(root.clone())]
        })
        .flatten()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    entries.push(SYSTEM_SANDBOX_PATH.to_owned());
    entries.join(":")
}

fn add_mount_target_directories(command: &mut Command, root: &Path) {
    let mut ancestors = root
        .ancestors()
        .skip(usize::from(!root.is_dir()))
        .take_while(|path| *path != Path::new("/"))
        .collect::<Vec<_>>();
    ancestors.reverse();
    for directory in ancestors {
        command.arg("--dir").arg(directory);
    }
}

struct SandboxOutputChannels {
    stdout: File,
    stderr: File,
    stdout_fd: libc::c_int,
    stderr_fd: libc::c_int,
}

impl SandboxOutputChannels {
    fn finish(self, launcher: Command) -> SandboxedCommand {
        SandboxedCommand {
            launcher,
            stdout: self.stdout,
            stderr: self.stderr,
        }
    }
}

#[cfg(target_os = "linux")]
fn prepare_sandbox_output(
    command: &mut Command,
) -> Result<SandboxOutputChannels, SandboxUnavailableReason> {
    use std::os::fd::AsRawFd as _;

    let (stdout, stdout_writer) = sandbox_output_pipe()?;
    let (stderr, stderr_writer) = sandbox_output_pipe()?;
    let stdout_fd = stdout_writer.as_raw_fd();
    let stderr_fd = stderr_writer.as_raw_fd();
    whitelist_sandbox_output_fds(command, stdout_writer, stderr_writer);
    Ok(SandboxOutputChannels {
        stdout,
        stderr,
        stdout_fd,
        stderr_fd,
    })
}

#[cfg(not(target_os = "linux"))]
fn prepare_sandbox_output(
    _command: &mut Command,
) -> Result<SandboxOutputChannels, SandboxUnavailableReason> {
    Err(SandboxUnavailableReason::UnsupportedPlatform)
}

#[cfg(target_os = "linux")]
fn sandbox_output_pipe() -> Result<(File, std::os::fd::OwnedFd), SandboxUnavailableReason> {
    use std::os::fd::FromRawFd as _;

    let mut descriptors = [-1; 2];
    // SAFETY: `descriptors` is writable storage for both new descriptors. The
    // atomic CLOEXEC flag prevents either end from leaking through a concurrent
    // spawn before Euler installs the exact child-side whitelist below.
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(SandboxUnavailableReason::CannotEnforce);
    }
    // SAFETY: a successful `pipe2` returned two distinct owned descriptors.
    let reader = unsafe { File::from_raw_fd(descriptors[0]) };
    // SAFETY: ownership of the second descriptor is independent of `reader`.
    let writer = unsafe { std::os::fd::OwnedFd::from_raw_fd(descriptors[1]) };
    let writer = move_output_writer_to_agent_range(writer)?;
    Ok((reader, writer))
}

#[cfg(target_os = "linux")]
fn move_output_writer_to_agent_range(
    writer: std::os::fd::OwnedFd,
) -> Result<std::os::fd::OwnedFd, SandboxUnavailableReason> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    if writer.as_raw_fd() >= FIRST_AGENT_OUTPUT_FD {
        return Ok(writer);
    }
    // SAFETY: `writer` is live. F_DUPFD_CLOEXEC creates an independent owned
    // descriptor in the dedicated agent-output range, so later child stdio
    // setup cannot overwrite the whitelisted pipe writer even when Euler
    // itself started with closed stdio.
    let descriptor = unsafe {
        libc::fcntl(
            writer.as_raw_fd(),
            libc::F_DUPFD_CLOEXEC,
            FIRST_AGENT_OUTPUT_FD,
        )
    };
    if descriptor < 0 {
        return Err(SandboxUnavailableReason::CannotEnforce);
    }
    // SAFETY: F_DUPFD_CLOEXEC returned a fresh owned descriptor.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(descriptor) })
}

/// Keep non-stdio host descriptors out of Bubblewrap and the agent command,
/// except for the two fresh write-only pipes that carry inner-command output.
/// A readable file or connected socket inherited from Euler would otherwise
/// bypass the mount and network boundary through `/proc/self/fd`.
///
/// `CLOEXEC` preserves Rust's private spawn-error pipe until `exec`, while
/// ensuring Bubblewrap and its inner command receive only standard I/O plus
/// the output-only whitelist. Linux 5.11+ can set the bit atomically with
/// `close_range`; older kernels use a post-fork `/proc/self/fd` syscall scan
/// and therefore remain supported.
#[cfg(target_os = "linux")]
fn whitelist_sandbox_output_fds(
    command: &mut Command,
    stdout: std::os::fd::OwnedFd,
    stderr: std::os::fd::OwnedFd,
) {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;

    // SAFETY: this hook performs only direct descriptor syscalls between fork
    // and exec. It neither allocates nor inspects shared process state.
    unsafe {
        command.pre_exec(move || {
            mark_all_inherited_fds_close_on_exec()?;
            clear_descriptor_close_on_exec(stdout.as_raw_fd())?;
            clear_descriptor_close_on_exec(stderr.as_raw_fd())
        });
    }
}

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
fn clear_descriptor_close_on_exec(descriptor: libc::c_int) -> std::io::Result<()> {
    // SAFETY: `descriptor` is one of the two live pipe writers retained by the
    // pre-exec closure. These calls inspect and clear only its CLOEXEC bit.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: preserves every descriptor flag except FD_CLOEXEC on the same
    // output-only pipe writer.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } != 0 {
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
    use std::fs;
    #[cfg(target_os = "linux")]
    use std::io::Read as _;
    #[cfg(target_os = "linux")]
    use std::net::TcpListener;
    #[cfg(target_os = "linux")]
    use std::os::fd::{AsRawFd, FromRawFd};
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::PermissionsExt as _;
    #[cfg(target_os = "linux")]
    use std::time::Duration;

    #[cfg(target_os = "linux")]
    fn command_arguments(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[cfg(target_os = "linux")]
    fn sandbox_writer_fds(command: &Command) -> [libc::c_int; 2] {
        let arguments = command_arguments(command);
        let wrapper = arguments
            .iter()
            .position(|argument| argument == SANDBOX_READY_WRAPPER)
            .expect("sandbox readiness wrapper argument");
        [
            arguments[wrapper + 2].parse().expect("stdout writer fd"),
            arguments[wrapper + 3].parse().expect("stderr writer fd"),
        ]
    }

    #[cfg(target_os = "linux")]
    fn run_sandbox_test_command(sandboxed: SandboxedCommand) -> std::process::Output {
        let (mut launcher, mut stdout, mut stderr) = sandboxed.into_parts();
        launcher
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = launcher.spawn().expect("spawn sandbox launcher");
        // The pre-exec closure owns the parent copies of the pipe writers.
        // Drop it after spawning so only the child-side copies hold the pipes.
        drop(launcher);
        let (status, stdout, stderr) = std::thread::scope(|scope| {
            let stdout_task = scope.spawn(move || {
                let mut output = String::new();
                stdout.read_to_string(&mut output).map(|_| output)
            });
            let stderr_task = scope.spawn(move || {
                let mut output = String::new();
                stderr.read_to_string(&mut output).map(|_| output)
            });
            let status = child.wait().expect("wait sandbox launcher");
            let stdout = stdout_task
                .join()
                .expect("stdout reader")
                .expect("read agent stdout");
            let stderr = stderr_task
                .join()
                .expect("stderr reader")
                .expect("read agent stderr");
            (status, stdout, stderr)
        });
        let stdout = strip_sandbox_stdout_ready_marker(&stdout)
            .expect("inner stdout readiness")
            .as_bytes()
            .to_vec();
        let stderr = strip_sandbox_stderr_ready_marker(&stderr)
            .expect("inner stderr readiness")
            .as_bytes()
            .to_vec();
        std::process::Output {
            status,
            stdout,
            stderr,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn profile_construction_freezes_topology_before_the_complete_probe_walk() {
        let workspace = tempfile::tempdir().expect("workspace");
        let _listener = std::os::unix::net::UnixListener::bind(workspace.path().join("host.sock"))
            .expect("host socket");
        let writable_roots = vec![workspace
            .path()
            .canonicalize()
            .expect("canonical workspace")];
        let system_runtime_mounts =
            canonical_system_runtime_mounts().expect("system runtime mounts");

        assert!(freeze_authority_roots(&writable_roots, &[], &system_runtime_mounts).is_ok());
        assert_eq!(
            inspect_authority_roots(&writable_roots, &[], &system_runtime_mounts),
            Err(SandboxUnavailableReason::UnsafeSpecialNode)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn profile_uses_private_root_workspace_bind_and_network_namespace() {
        let temp = tempfile::tempdir().expect("temp workspace");
        let workspace = temp.path().canonicalize().expect("canonical workspace");
        let system_runtime_mounts =
            canonical_system_runtime_mounts().expect("system runtime mounts");
        let sandboxed = bwrap_command(
            Path::new("/usr/bin/bwrap"),
            SandboxMountPlan {
                writable_roots: std::slice::from_ref(&workspace),
                runtime_roots: &[],
                system_runtime_mounts: &system_runtime_mounts,
                read_only_surfaces: &[],
            },
            OsStr::new("/bin/sh"),
            ["-c", "true"],
        )
        .expect("prepare sandbox command");
        let (command, _stdout, _stderr) = sandboxed.into_parts();
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
                    workspace.to_string_lossy().as_ref(),
                ]
        }));
        assert!(arguments
            .windows(3)
            .any(|triple| triple == ["--tmpfs", "/tmp", "--dir"]));
        assert!(arguments
            .windows(3)
            .any(|triple| triple == ["--ro-bind", "/usr/bin", "/usr/bin"]));
        assert!(!arguments
            .windows(3)
            .any(|triple| triple == ["--ro-bind", "/usr", "/usr"]));
        assert!(!arguments
            .windows(3)
            .any(|triple| triple == ["--ro-bind", "/usr/share", "/usr/share"]));
        assert!(!arguments.windows(3).any(|triple| {
            triple[0] == "--ro-bind"
                && (triple[1] == "/usr/local"
                    || triple[1].starts_with("/usr/local/")
                    || triple[2] == "/usr/local"
                    || triple[2].starts_with("/usr/local/"))
        }));
        assert!(!arguments
            .windows(3)
            .any(|triple| triple == ["--ro-bind", "/", "/"]));
        assert!(!arguments
            .windows(3)
            .any(|triple| triple == ["--ro-bind", "/etc", "/etc"]));
        let inner = arguments
            .iter()
            .skip_while(|argument| argument.as_str() != "--")
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            &inner[..5],
            &[
                "--",
                "/bin/sh",
                "-c",
                SANDBOX_READY_WRAPPER,
                "euler-sandbox"
            ]
        );
        let [stdout_fd, stderr_fd] = sandbox_writer_fds(&command);
        assert!(
            stdout_fd >= FIRST_AGENT_OUTPUT_FD
                && stderr_fd >= FIRST_AGENT_OUTPUT_FD
                && stdout_fd != stderr_fd
        );
        assert_eq!(&inner[7..], &["/bin/sh", "-c", "true"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn repository_worktree_collection_is_overlaid_read_only() {
        let temp = tempfile::tempdir().expect("temp workspace");
        let workspace = temp.path().canonicalize().expect("canonical workspace");
        let worktrees = workspace.join(".worktrees");
        fs::create_dir(&worktrees).expect("worktree collection");
        let system_runtime_mounts =
            canonical_system_runtime_mounts().expect("system runtime mounts");
        let before = crate::file_diff::capture_workspace_snapshot(&workspace)
            .expect("pre-command workspace snapshot");
        let read_only_surfaces = before
            .frozen_read_only_surfaces()
            .cloned()
            .collect::<Vec<_>>();
        let sandboxed = bwrap_command(
            Path::new("/usr/bin/bwrap"),
            SandboxMountPlan {
                writable_roots: std::slice::from_ref(&workspace),
                runtime_roots: &[],
                system_runtime_mounts: &system_runtime_mounts,
                read_only_surfaces: &read_only_surfaces,
            },
            OsStr::new("/bin/sh"),
            ["-c", "true"],
        )
        .expect("prepare sandbox command");
        let (command, _stdout, _stderr) = sandboxed.into_parts();
        let arguments = command_arguments(&command);
        let worktrees = worktrees.to_string_lossy();

        assert!(arguments
            .windows(3)
            .any(|triple| { triple == ["--ro-bind", worktrees.as_ref(), worktrees.as_ref()] }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn frozen_worktree_identity_rejects_removal_and_replacement_before_launch() {
        let temp = tempfile::tempdir().expect("temp workspace");
        let workspace = temp.path().canonicalize().expect("canonical workspace");
        let worktrees = workspace.join(".worktrees");
        let original = workspace.join("original-worktrees");
        fs::create_dir(&worktrees).expect("worktree collection");
        let sandbox =
            WorkspaceSandbox::new(&workspace, &[], &[], SandboxProfile::WorkspaceNoNetwork);
        let profile_is_enforced = sandbox.availability().is_enforced();
        let before = crate::file_diff::capture_workspace_snapshot(&workspace)
            .expect("pre-command workspace snapshot");
        let frozen = before
            .frozen_read_only_surfaces()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(frozen.len(), 1);

        fs::rename(&worktrees, &original).expect("retain original directory identity");
        fs::create_dir(&worktrees).expect("replacement worktree collection");
        assert_eq!(
            validate_frozen_read_only_surfaces(std::slice::from_ref(&workspace), &frozen),
            Err(SandboxUnavailableReason::UnsafeMountTopology)
        );
        if profile_is_enforced {
            assert!(matches!(
                sandbox.command(&frozen, "/bin/sh", ["-c", "true"]),
                Err(SandboxUnavailableReason::UnsafeMountTopology)
            ));
        }
        assert_eq!(
            crate::file_diff::recapture_workspace_snapshot(&before),
            Err(crate::file_diff::WorkspaceSnapshotError::ProtectedSurfaceIdentity)
        );

        fs::remove_dir(&worktrees).expect("remove replacement collection");
        assert!(
            validate_frozen_read_only_surfaces(std::slice::from_ref(&workspace), &frozen).is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn command_construction_does_not_rediscover_a_new_worktree_collection() {
        let temp = tempfile::tempdir().expect("temp workspace");
        let workspace = temp.path().canonicalize().expect("canonical workspace");
        let before = crate::file_diff::capture_workspace_snapshot(&workspace)
            .expect("pre-command workspace snapshot");
        let frozen = before
            .frozen_read_only_surfaces()
            .cloned()
            .collect::<Vec<_>>();
        assert!(frozen.is_empty());
        let worktrees = workspace.join(".worktrees");
        fs::create_dir(&worktrees).expect("new worktree collection");

        let system_runtime_mounts =
            canonical_system_runtime_mounts().expect("system runtime mounts");
        let sandboxed = bwrap_command(
            Path::new("/usr/bin/bwrap"),
            SandboxMountPlan {
                writable_roots: std::slice::from_ref(&workspace),
                runtime_roots: &[],
                system_runtime_mounts: &system_runtime_mounts,
                read_only_surfaces: &frozen,
            },
            OsStr::new("/bin/sh"),
            ["-c", "true"],
        )
        .expect("prepare sandbox command");
        let (command, _stdout, _stderr) = sandboxed.into_parts();
        let arguments = command_arguments(&command);
        let worktrees = worktrees.to_string_lossy();

        assert!(!arguments
            .windows(3)
            .any(|triple| triple == ["--ro-bind", worktrees.as_ref(), worktrees.as_ref()]));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fake_launcher_output_never_reaches_dedicated_agent_pipes() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let launcher = temp.path().join("fake-bwrap");
        fs::write(
            &launcher,
            "#!/bin/sh\n\
             printf 'launcher stdout before readiness\\n'\n\
             printf 'launcher stderr before readiness\\n' >&2\n\
             while [ \"$#\" -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n\
             [ \"$#\" -gt 0 ] || exit 126\n\
             shift\n\
             \"$@\"\n\
             status=$?\n\
             printf 'launcher stdout after readiness\\n'\n\
             printf 'launcher stderr after readiness\\n' >&2\n\
             exit \"$status\"\n",
        )
        .expect("fake launcher");
        let mut permissions = fs::metadata(&launcher)
            .expect("launcher metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&launcher, permissions).expect("executable launcher");

        let inherited = File::open("/dev/null").expect("host descriptor canary");
        let inherited_fd = inherited.as_raw_fd();
        // SAFETY: the descriptor remains owned by `inherited`; this only makes
        // the regression canary inheritable before the production pre-exec
        // whitelist closes it again.
        unsafe {
            let flags = libc::fcntl(inherited_fd, libc::F_GETFD);
            assert!(flags >= 0, "read canary flags");
            assert_eq!(
                libc::fcntl(inherited_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC),
                0,
                "clear canary CLOEXEC"
            );
        }
        let script = format!(
            "if test -e /proc/self/fd/{inherited_fd}; then printf inherited-host-fd; exit 97; fi; printf 'child stdout\\n'; printf 'child stderr\\n' >&2"
        );
        let system_runtime_mounts =
            canonical_system_runtime_mounts().expect("system runtime mounts");
        let sandboxed = bwrap_command(
            &launcher,
            SandboxMountPlan {
                writable_roots: std::slice::from_ref(&workspace),
                runtime_roots: &[],
                system_runtime_mounts: &system_runtime_mounts,
                read_only_surfaces: &[],
            },
            OsStr::new("/bin/sh"),
            ["-c", script.as_str()],
        )
        .expect("prepare fake launcher command");
        let (mut launcher, mut stdout, mut stderr) = sandboxed.into_parts();
        let [stdout_fd, stderr_fd] = sandbox_writer_fds(&launcher);
        assert!(
            stdout_fd >= FIRST_AGENT_OUTPUT_FD && stderr_fd >= FIRST_AGENT_OUTPUT_FD,
            "the regression must exercise dash with multi-digit readiness descriptors"
        );
        launcher
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let launcher_output = launcher.output().expect("run fake launcher");
        drop(launcher);
        assert!(launcher_output.status.success(), "{launcher_output:?}");

        let mut agent_stdout = String::new();
        let mut agent_stderr = String::new();
        stdout
            .read_to_string(&mut agent_stdout)
            .expect("agent stdout");
        stderr
            .read_to_string(&mut agent_stderr)
            .expect("agent stderr");
        let tool_output = format!(
            "{}{}",
            strip_sandbox_stdout_ready_marker(&agent_stdout).expect("stdout readiness"),
            strip_sandbox_stderr_ready_marker(&agent_stderr).expect("stderr readiness")
        );
        assert_eq!(tool_output, "child stdout\nchild stderr\n");
        assert!(!tool_output.contains("launcher"));
        assert!(!tool_output.contains("inherited-host-fd"));
        assert!(String::from_utf8_lossy(&launcher_output.stdout).contains("before readiness"));
        assert!(String::from_utf8_lossy(&launcher_output.stdout).contains("after readiness"));
        assert!(String::from_utf8_lossy(&launcher_output.stderr).contains("before readiness"));
        assert!(String::from_utf8_lossy(&launcher_output.stderr).contains("after readiness"));
    }

    #[test]
    fn writable_and_runtime_roots_must_be_bounded_and_non_overlapping() {
        let temp = tempfile::tempdir().expect("temp dir");
        let primary = temp.path().join("primary");
        let attached = temp.path().join("attached");
        let runtime = temp.path().join("runtime");
        let ambiguous_runtime = temp.path().join("runtime:split");
        fs::create_dir_all(primary.join("nested")).expect("primary");
        fs::create_dir(&attached).expect("attached");
        fs::create_dir(&runtime).expect("runtime");
        fs::create_dir(&ambiguous_runtime).expect("ambiguous runtime");

        let writable = canonical_writable_roots(&primary, std::slice::from_ref(&attached))
            .expect("valid writable roots");
        assert_eq!(writable.len(), 2);
        assert!(canonical_writable_roots(&primary, &[primary.join("nested")]).is_err());
        assert!(canonical_writable_roots(&primary, &[PathBuf::from("/")]).is_err());
        assert!(canonical_runtime_roots(&writable, &[runtime]).is_ok());
        assert!(canonical_runtime_roots(&writable, &[primary]).is_err());
        assert!(canonical_runtime_roots(&writable, &[ambiguous_runtime]).is_err());
    }

    #[test]
    fn roots_cannot_replace_private_sandbox_mount_targets() {
        for target in PRIVATE_SANDBOX_MOUNT_TARGETS {
            assert!(overrides_private_sandbox_mount(Path::new(target)));
            assert!(!overrides_private_sandbox_mount(
                &Path::new(target).join("explicit-descendant")
            ));
        }

        let workspace = tempfile::tempdir().expect("workspace");
        let writable = canonical_writable_roots(workspace.path(), &[])
            .expect("a /tmp descendant remains a valid writable root");
        assert!(canonical_writable_roots(Path::new("/tmp"), &[]).is_err());
        assert!(canonical_writable_roots(workspace.path(), &[PathBuf::from("/proc")]).is_err());
        assert!(
            canonical_runtime_roots_for_home(&writable, &[PathBuf::from("/dev")], None,).is_err()
        );

        #[cfg(target_os = "linux")]
        {
            assert_eq!(
                WorkspaceSandbox::new("/tmp", &[], &[], SandboxProfile::WorkspaceNoNetwork,)
                    .availability(),
                SandboxAvailability::Unavailable(SandboxUnavailableReason::InvalidWritableRoots)
            );
            assert_eq!(
                WorkspaceSandbox::new(
                    workspace.path(),
                    &[PathBuf::from("/proc")],
                    &[],
                    SandboxProfile::WorkspaceNoNetwork,
                )
                .availability(),
                SandboxAvailability::Unavailable(SandboxUnavailableReason::InvalidWritableRoots)
            );
            assert_eq!(
                WorkspaceSandbox::new(
                    workspace.path(),
                    &[],
                    &[PathBuf::from("/dev")],
                    SandboxProfile::WorkspaceNoNetwork,
                )
                .availability(),
                SandboxAvailability::Unavailable(SandboxUnavailableReason::InvalidRuntimeRoots)
            );
        }

        let runtime = tempfile::tempdir().expect("runtime root");
        assert_eq!(
            canonical_runtime_roots_for_home(&writable, &[runtime.path().to_path_buf()], None,)
                .expect("an explicit /tmp descendant remains bounded runtime authority"),
            vec![runtime.path().canonicalize().expect("canonical runtime")]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn enforced_profile_accepts_explicit_descendants_of_private_tmp() {
        let workspace = tempfile::tempdir_in("/tmp").expect("workspace");
        let runtime = tempfile::tempdir_in("/tmp").expect("runtime root");
        fs::write(runtime.path().join("readable"), "runtime").expect("runtime fixture");
        let sandbox = WorkspaceSandbox::new(
            workspace.path(),
            &[],
            &[runtime.path().to_path_buf()],
            SandboxProfile::WorkspaceNoNetwork,
        );
        if !sandbox.availability().is_enforced() {
            return;
        }
        let script = format!(
            "test \"$(cat {})\" = runtime; printf workspace > marker; if printf bad > {}/changed; then exit 1; fi",
            shell_quote(&runtime.path().join("readable")),
            shell_quote(runtime.path())
        );
        let output = run_sandbox_test_command(
            sandbox
                .command(&[], "/bin/sh", ["-c", script.as_str()])
                .expect("sandbox command"),
        );

        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            fs::read_to_string(workspace.path().join("marker")).expect("workspace marker"),
            "workspace"
        );
        assert!(!runtime.path().join("changed").exists());
    }

    #[test]
    fn runtime_roots_reject_the_configured_home_and_its_ancestors() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        let home_parent = temp.path().join("homes");
        let home = home_parent.join("user");
        let prepared_runtime = home.join("toolchains").join("rust");
        fs::create_dir(&workspace).expect("workspace");
        fs::create_dir(&home_parent).expect("home parent");
        fs::create_dir(&home).expect("home");
        fs::create_dir_all(&prepared_runtime).expect("prepared runtime");

        let writable = canonical_writable_roots(&workspace, &[]).expect("writable roots");
        let home = home.canonicalize().expect("canonical home");

        assert!(canonical_runtime_roots_for_home(
            &writable,
            std::slice::from_ref(&home),
            Some(&home),
        )
        .is_err());
        assert!(canonical_runtime_roots_for_home(&writable, &[home_parent], Some(&home)).is_err());
        assert_eq!(
            canonical_runtime_roots_for_home(
                &writable,
                std::slice::from_ref(&prepared_runtime),
                Some(&home),
            )
            .expect("an explicit prepared home subdirectory is bounded authority"),
            vec![prepared_runtime
                .canonicalize()
                .expect("canonical prepared runtime")]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_structured_authority_uses_owned_mounts_and_frozen_root_identity() {
        use std::os::fd::AsRawFd as _;

        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        let other = temp.path().join("other");
        fs::create_dir(&workspace).expect("workspace");
        fs::create_dir(&other).expect("other directory");
        fs::write(workspace.join("note.txt"), "content").expect("fixture file");
        let workspace = workspace.canonicalize().expect("canonical workspace");
        let authority = StructuredPathAuthority::new(vec![workspace.clone()]);

        authority.validate().expect("stable macOS authority");
        assert!(!macos_mount_table().expect("owned mount table").is_empty());
        let root = fs::File::open(&workspace).expect("open workspace root");
        authority
            .validate_opened_root(&workspace, root.as_raw_fd())
            .expect("opened root matches frozen identity");
        let child = fs::File::open(workspace.join("note.txt")).expect("open child");
        authority
            .validate_opened_candidate(&workspace, child.as_raw_fd())
            .expect("child remains on frozen mount");

        let substituted_root = fs::File::open(other).expect("open different directory");
        assert_eq!(
            authority.validate_opened_root(&workspace, substituted_root.as_raw_fd()),
            Err(SandboxUnavailableReason::UnsafeMountTopology)
        );
        assert_eq!(
            macos_fixed_array_bytes(&[b'x' as libc::c_char; 4]),
            Err(SandboxUnavailableReason::AuthorityInspectionFailed)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mount_identity_rejects_same_device_nested_bind_mount() {
        let table = parse_mount_table(
            "10 1 254:0 / / rw - ext4 /dev/root rw\n\
             11 10 254:0 /work /work rw - ext4 /dev/root rw\n\
             12 11 254:0 /other /work/nested rw - ext4 /dev/root rw\n",
        )
        .expect("mount table fixture");

        assert_eq!(
            table.validate_root(Path::new("/work")),
            Err(SandboxUnavailableReason::UnsafeMountTopology)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn aliased_proc_mount_is_rejected_by_filesystem_identity() {
        let table = parse_mount_table(
            "10 1 254:0 / / rw - ext4 /dev/root rw\n\
             11 10 0:5 /proc-subtree /ordinary-name rw - proc proc rw\n",
        )
        .expect("mount table fixture");

        assert_eq!(
            table.validate_root(Path::new("/ordinary-name/1")),
            Err(SandboxUnavailableReason::UnsafeMountTopology)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unapproved_filesystem_type_fails_closed() {
        let table = parse_mount_table(
            "10 1 254:0 / / rw - ext4 /dev/root rw\n\
             11 10 0:42 / /workspace rw - fuse.portal portal rw\n",
        )
        .expect("mount table fixture");

        assert_eq!(
            table.validate_root(Path::new("/workspace")),
            Err(SandboxUnavailableReason::UnsafeMountTopology)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unrelated_namespace_mount_root_does_not_poison_authority_table() {
        let table = parse_mount_table(
            "10 1 254:0 / / rw - ext4 /dev/root rw\n\
             11 10 254:0 /work /work rw - ext4 /dev/root rw\n\
             12 10 0:4 net:[4026532267] /run/docker/netns/example rw - nsfs nsfs rw\n",
        )
        .expect("namespace descriptors are valid mount roots");

        assert_eq!(
            table.validate_root(Path::new("/work")),
            Ok((11, PathBuf::from("/work")))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn malformed_or_ambiguous_mountinfo_fails_inspection() {
        let relative = "10 1 254:0 / relative rw - ext4 /dev/root rw\n";
        let duplicate_id = "10 1 254:0 / / rw - ext4 /dev/root rw\n\
                            10 1 254:0 /other /other rw - ext4 /dev/root rw\n";
        let non_normal = "10 1 254:0 / /work/../escape rw - ext4 /dev/root rw\n";
        let invalid_escape = "10 1 254:0 / /work\\057escape rw - ext4 /dev/root rw\n";

        assert!(matches!(
            parse_mount_table(relative),
            Err(SandboxUnavailableReason::AuthorityInspectionFailed)
        ));
        assert!(matches!(
            parse_mount_table(duplicate_id),
            Err(SandboxUnavailableReason::AuthorityInspectionFailed)
        ));
        assert!(matches!(
            parse_mount_table(non_normal),
            Err(SandboxUnavailableReason::AuthorityInspectionFailed)
        ));
        assert!(matches!(
            parse_mount_table(invalid_escape),
            Err(SandboxUnavailableReason::AuthorityInspectionFailed)
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_magic_link_cannot_be_selected_as_a_root_alias() {
        assert_eq!(
            open_without_magic_links(Path::new("/proc/self/root")),
            Err(SandboxUnavailableReason::UnsafeMountTopology)
        );
    }

    #[test]
    fn missing_readiness_marker_returns_only_a_safe_reason() {
        let unframed_bytes = "not verified as inner command output";

        assert_eq!(
            strip_sandbox_stdout_ready_marker(unframed_bytes),
            Err(SandboxUnavailableReason::CannotEnforce)
        );
        assert_eq!(
            strip_sandbox_stderr_ready_marker(unframed_bytes),
            Err(SandboxUnavailableReason::CannotEnforce)
        );
    }

    #[test]
    fn successful_readiness_strips_any_unverified_prefix_from_both_streams() {
        assert_eq!(
            strip_sandbox_stdout_ready_marker(
                "unverified\0__EULER_SANDBOX_STDOUT_READY__\0child stdout"
            ),
            Ok("child stdout")
        );
        assert_eq!(
            strip_sandbox_stderr_ready_marker(
                "unverified\0__EULER_SANDBOX_STDERR_READY__\0child stderr"
            ),
            Ok("child stderr")
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
    fn invalid_workspace_fails_closed_before_bubblewrap_is_invoked() {
        let temp = tempfile::tempdir().expect("temp dir");
        let missing = temp.path().join("missing");
        let sandbox = WorkspaceSandbox::new(&missing, &[], &[], SandboxProfile::WorkspaceNoNetwork);

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
            sandbox.command(&[], "/bin/sh", ["-c", "true"]).map(|_| ()),
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
        let sandbox =
            WorkspaceSandbox::new(&workspace, &[], &[], SandboxProfile::WorkspaceNoNetwork);
        if !sandbox.availability().is_enforced() {
            return;
        }

        let secret = shell_quote(&secret);
        let escape = shell_quote(&escape);
        let inside = shell_quote(&workspace.join("inside.txt"));
        let script = format!(
            "printf inside > {inside}; test ! -e {secret}; if echo outside > {escape}; then exit 1; fi"
        );
        let output = run_sandbox_test_command(
            sandbox
                .command(&[], "/bin/sh", ["-c", script.as_str()])
                .expect("enforced sandbox command"),
        );

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

    #[cfg(target_os = "linux")]
    #[test]
    fn canonical_root_substitution_blocks_later_launch() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        let original = temp.path().join("original-workspace");
        let outside = temp.path().join("outside");
        fs::create_dir(&workspace).expect("workspace");
        fs::create_dir(&outside).expect("outside");
        let sandbox =
            WorkspaceSandbox::new(&workspace, &[], &[], SandboxProfile::WorkspaceNoNetwork);
        if !sandbox.availability().is_enforced() {
            return;
        }

        fs::rename(&workspace, &original).expect("move selected root");
        symlink(&outside, &workspace).expect("substitute selected root");
        assert!(matches!(
            sandbox.command(&[], "/bin/sh", ["-c", "printf bad > escaped"]),
            Err(SandboxUnavailableReason::AuthorityInspectionFailed)
        ));
        assert!(!outside.join("escaped").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn enforced_profile_writes_every_attached_root_and_no_sibling() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        let attached = temp.path().join("attached");
        let outside = temp.path().join("outside");
        fs::create_dir(&workspace).expect("workspace");
        fs::create_dir(&attached).expect("attached");
        fs::create_dir(&outside).expect("outside");
        let sandbox = WorkspaceSandbox::new(
            &workspace,
            std::slice::from_ref(&attached),
            &[],
            SandboxProfile::WorkspaceNoNetwork,
        );
        if !sandbox.availability().is_enforced() {
            return;
        }

        let primary_file = shell_quote(&workspace.join("primary.txt"));
        let attached_file = shell_quote(&attached.join("attached.txt"));
        let outside_file = shell_quote(&outside.join("escape.txt"));
        let script = format!(
            "printf primary > {primary_file}; printf attached > {attached_file}; if printf escape > {outside_file}; then exit 1; fi"
        );
        let output = run_sandbox_test_command(
            sandbox
                .command(&[], "/bin/sh", ["-c", script.as_str()])
                .expect("sandbox command"),
        );

        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            fs::read_to_string(workspace.join("primary.txt")).unwrap(),
            "primary"
        );
        assert_eq!(
            fs::read_to_string(attached.join("attached.txt")).unwrap(),
            "attached"
        );
        assert!(!outside.join("escape.txt").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn enforced_profile_can_read_but_not_mutate_worktree_collection() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        let collection = workspace.join(".worktrees");
        fs::create_dir_all(&collection).expect("worktree collection");
        let protected = collection.join("protected.txt");
        fs::write(&protected, "readable").expect("protected fixture");
        let sandbox =
            WorkspaceSandbox::new(&workspace, &[], &[], SandboxProfile::WorkspaceNoNetwork);
        if !sandbox.availability().is_enforced() {
            return;
        }
        let read_only_surfaces =
            freeze_read_only_workspace_surfaces(&workspace).expect("freeze worktree collection");

        let protected_argument = shell_quote(&protected);
        let created_argument = shell_quote(&collection.join("created.txt"));
        let script = format!(
            "test \"$(cat {protected_argument})\" = readable; if printf bad > {protected_argument}; then exit 1; fi; if printf bad > {created_argument}; then exit 1; fi"
        );
        let output = run_sandbox_test_command(
            sandbox
                .command(&read_only_surfaces, "/bin/sh", ["-c", script.as_str()])
                .expect("sandbox command"),
        );

        assert!(output.status.success(), "{output:?}");
        assert_eq!(fs::read_to_string(&protected).unwrap(), "readable");
        assert!(!collection.join("created.txt").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn explicit_read_only_rust_toolchain_runs_cargo_without_becoming_writable() {
        let Ok(sysroot_output) = Command::new("rustc").args(["--print", "sysroot"]).output() else {
            return;
        };
        if !sysroot_output.status.success() {
            return;
        }
        let sysroot = PathBuf::from(String::from_utf8_lossy(&sysroot_output.stdout).trim());
        if !sysroot.join("bin/cargo").is_file() || sysroot.starts_with("/usr") {
            return;
        }
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let sandbox = WorkspaceSandbox::new(
            &workspace,
            &[],
            std::slice::from_ref(&sysroot),
            SandboxProfile::WorkspaceNoNetwork,
        );
        if !sandbox.availability().is_enforced() {
            return;
        }
        let marker = sysroot.join("euler-must-not-write");
        let script = format!(
            "cargo --version && rustc --version && if printf bad > {}; then exit 1; fi",
            shell_quote(&marker)
        );
        let output = run_sandbox_test_command(
            sandbox
                .command(&[], "/bin/sh", ["-c", script.as_str()])
                .expect("sandbox command"),
        );

        assert!(output.status.success(), "{output:?}");
        assert!(!marker.exists());
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
        let sandbox =
            WorkspaceSandbox::new(&workspace, &[], &[], SandboxProfile::WorkspaceNoNetwork);
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
        let output = run_sandbox_test_command(
            sandbox
                .command(&[], "/usr/bin/python3", ["-c", script.as_str()])
                .expect("enforced sandbox command"),
        );

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

    #[cfg(target_os = "linux")]
    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }
}

#[cfg(test)]
#[path = "sandbox_test.rs"]
mod sandbox_test;
