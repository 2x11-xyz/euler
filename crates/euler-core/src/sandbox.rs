//! Linux workspace subprocess sandboxing with Bubblewrap.
//!
//! The profile is deliberately narrow: an agent-controlled child sees its
//! workspace, a private runtime, and no host home or network. It is an
//! execution boundary, not a synonym for permission approval.

use std::ffi::OsStr;
use std::fmt;
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
    pub const fn label(self) -> &'static str {
        match self {
            Self::WorkspaceNoNetwork => "sandboxed workspace (network disabled)",
        }
    }
}

/// Whether agent-controlled subprocesses use a sandbox profile.
///
/// This is a core execution choice, intentionally separate from the
/// capability gate and its approval modes. Disabled remains the default until
/// a user-facing mode can truthfully activate an enforced profile.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SubprocessSandbox {
    #[default]
    Disabled,
    Enforce(SandboxProfile),
}

/// A concise, non-secret reason why a requested sandbox profile cannot run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxUnavailableReason {
    /// Euler is not running on a Linux host.
    UnsupportedPlatform,
    /// The `bwrap` executable was not found.
    BubblewrapMissing,
    /// Bubblewrap could not create the profile that Euler requires.
    CannotEnforce,
    /// The selected workspace cannot be resolved to a directory.
    InvalidWorkspace,
}

impl SandboxUnavailableReason {
    pub const fn message(self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => "workspace sandbox is currently supported on Linux only",
            Self::BubblewrapMissing => {
                "workspace sandbox requires Bubblewrap (`bwrap`) to be installed"
            }
            Self::CannotEnforce => {
                "this host cannot enforce Euler's required workspace sandbox profile"
            }
            Self::InvalidWorkspace => {
                "workspace sandbox requires an accessible workspace directory"
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

/// A workspace-specific profile that has already been probed. It retains the
/// stable availability result so callers can fail closed without copying raw
/// Bubblewrap diagnostics into tool output or provenance.
#[derive(Clone, Debug)]
pub(crate) struct WorkspaceSandbox {
    workspace: Option<PathBuf>,
    bwrap: Option<PathBuf>,
    availability: SandboxAvailability,
}

impl WorkspaceSandbox {
    /// Build and probe a profile for one workspace. Construction itself never
    /// falls back to host execution: callers must inspect or propagate the
    /// resulting [`SandboxAvailability`].
    pub(crate) fn new(workspace: impl AsRef<Path>, profile: SandboxProfile) -> Self {
        if !cfg!(target_os = "linux") {
            return Self {
                workspace: None,
                bwrap: None,
                availability: SandboxAvailability::Unavailable(
                    SandboxUnavailableReason::UnsupportedPlatform,
                ),
            };
        }
        let Ok(workspace) = canonical_workspace(workspace.as_ref()) else {
            return Self {
                workspace: None,
                bwrap: None,
                availability: SandboxAvailability::Unavailable(
                    SandboxUnavailableReason::InvalidWorkspace,
                ),
            };
        };
        let Some(bwrap) = bwrap_path() else {
            return Self {
                workspace: Some(workspace),
                bwrap: None,
                availability: SandboxAvailability::Unavailable(
                    SandboxUnavailableReason::BubblewrapMissing,
                ),
            };
        };
        let availability = probe_profile(&bwrap, &workspace, profile);
        Self {
            workspace: Some(workspace),
            bwrap: Some(bwrap),
            availability,
        }
    }

    pub(crate) const fn availability(&self) -> SandboxAvailability {
        self.availability
    }

    /// Wrap one program invocation in the enforced profile. The caller owns
    /// stdio and timeout configuration on the returned command. An
    /// unavailable profile returns its concise public reason and never gives
    /// the caller an unsandboxed command.
    pub(crate) fn command<I, S>(
        &self,
        program: impl AsRef<OsStr>,
        args: I,
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
        let bwrap = self
            .bwrap
            .as_deref()
            .ok_or(SandboxUnavailableReason::CannotEnforce)?;
        Ok(bwrap_command(
            bwrap,
            profile,
            workspace,
            program.as_ref(),
            args,
        ))
    }
}

const BWRAP_PATHS: &[&str] = &["/usr/bin/bwrap", "/bin/bwrap"];
const SANDBOX_WORKSPACE: &str = "/workspace";
const SANDBOX_HOME: &str = "/tmp/home";
const SANDBOX_CACHE: &str = "/tmp/cache";
const RUNTIME_MOUNTS: &[&str] = &["/usr", "/bin", "/lib", "/lib64"];
const SANDBOX_PATH: &str = "/usr/bin:/bin";
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const SANDBOX_READY_MARKER: &str = "__EULER_SANDBOX_READY__\n";
const SANDBOX_READY_WRAPPER: &str = "printf '__EULER_SANDBOX_READY__\\n'; exec \"$@\"";
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

/// Probe whether the default profile is actually enforceable for `workspace`.
///
/// The child process gets a private root, can access its workspace, cannot see
/// `/home`, and must enter a network namespace. A failure is intentionally
/// collapsed to a stable public reason: raw Bubblewrap diagnostics may expose
/// host details and are not suitable for model-facing or transcript output.
pub fn probe_workspace_sandbox(workspace: &Path) -> SandboxAvailability {
    WorkspaceSandbox::new(workspace, SandboxProfile::WorkspaceNoNetwork).availability()
}

/// Remove the private prelude emitted only after Bubblewrap has completed its
/// setup and entered the inner command. If it is absent, the launcher failed
/// before the child started, so callers must not surface its raw diagnostics.
pub(crate) fn strip_sandbox_ready_marker(stdout: &str) -> Result<&str, SandboxUnavailableReason> {
    stdout
        .strip_prefix(SANDBOX_READY_MARKER)
        .ok_or(SandboxUnavailableReason::CannotEnforce)
}

fn probe_profile(bwrap: &Path, workspace: &Path, profile: SandboxProfile) -> SandboxAvailability {
    let mut command = bwrap_command(
        bwrap,
        profile,
        workspace,
        OsStr::new("/bin/sh"),
        [
            "-c",
            "test -w /workspace && test ! -e /home && test -d /usr",
        ],
    );
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

fn bwrap_command<I, S>(
    bwrap: &Path,
    profile: SandboxProfile,
    workspace: &Path,
    program: &OsStr,
    args: I,
) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(bwrap);
    // Clear the launcher too: `--clearenv` protects the inner command, while
    // this prevents an inherited loader/configuration variable from changing
    // Bubblewrap before it establishes the namespace.
    command.env_clear();
    mark_inherited_fds_close_on_exec(&mut command);
    command.args([
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
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
        SANDBOX_PATH,
        "--tmpfs",
        "/",
    ]);
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
        "--dir",
        SANDBOX_WORKSPACE,
        "--bind",
    ]);
    command
        .arg(workspace)
        .arg(SANDBOX_WORKSPACE)
        .args(["--chdir", SANDBOX_WORKSPACE, "--", "/bin/sh", "-c"])
        .arg(SANDBOX_READY_WRAPPER)
        .arg("euler-sandbox")
        .arg(program)
        .args(args);
    command
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
    fn profile_uses_private_root_workspace_bind_and_network_namespace() {
        let temp = tempfile::tempdir().expect("temp workspace");
        let workspace = temp.path().canonicalize().expect("canonical workspace");
        let command = bwrap_command(
            Path::new("/usr/bin/bwrap"),
            SandboxProfile::WorkspaceNoNetwork,
            &workspace,
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
        assert!(!arguments
            .windows(3)
            .any(|triple| triple == ["--ro-bind", "/etc", "/etc"]));
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
    fn invalid_workspace_fails_closed_before_bubblewrap_is_invoked() {
        let temp = tempfile::tempdir().expect("temp dir");
        let missing = temp.path().join("missing");
        let sandbox = WorkspaceSandbox::new(&missing, SandboxProfile::WorkspaceNoNetwork);

        assert_eq!(
            sandbox.availability(),
            SandboxAvailability::Unavailable(SandboxUnavailableReason::InvalidWorkspace)
        );
        assert!(matches!(
            sandbox.command("/bin/sh", ["-c", "true"]),
            Err(SandboxUnavailableReason::InvalidWorkspace)
        ));
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
            .command("/bin/sh", ["-c", script.as_str()])
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
            .command("/usr/bin/python3", ["-c", script.as_str()])
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

    #[cfg(target_os = "linux")]
    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }
}

#[cfg(test)]
#[path = "sandbox_test.rs"]
mod sandbox_test;
