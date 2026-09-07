//! Linux workspace subprocess sandboxing with Bubblewrap.
//!
//! The profile is deliberately narrow: an agent-controlled child sees its
//! workspace, a private runtime, the toolchain roots its host environment
//! implies, and no host home or network. It is an execution boundary, not a
//! synonym for permission approval.
//!
//! Bubblewrap is the default and enforced backend on Linux (ADR 0021 row A′).
//! Off Linux there is no backend yet, so agent subprocesses run on the host
//! under the ordinary permission decider until the Seatbelt backend lands;
//! [`SandboxBackend`] names both so a third backend slots in without touching
//! call sites.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
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

/// The execution boundary that actually ran, or would run, an agent command.
///
/// This is the seam the macOS Seatbelt backend slots into: call sites record
/// and branch on the backend, never on the host operating system.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxBackend {
    /// Linux Bubblewrap, the default and enforced backend.
    Bwrap,
    /// Direct host execution, gated only by the permission decider.
    Host,
}

impl SandboxBackend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bwrap => "bwrap",
            Self::Host => "host",
        }
    }
}

/// Whether agent-controlled subprocesses use a sandbox profile.
///
/// This is a core execution choice, intentionally separate from the
/// capability gate and its approval modes. Linux defaults to the enforced
/// no-network profile; every other platform runs on the host until its own
/// backend exists.
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
        if cfg!(target_os = "linux") {
            Self::Enforce(SandboxProfile::WorkspaceNoNetwork)
        } else {
            Self::Host
        }
    }
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

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::BubblewrapMissing => "bubblewrap_missing",
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
    /// A sysctl disables unprivileged user namespaces outright.
    UserNamespacesDisabled,
    /// Ubuntu 23.10+ AppArmor restricts unprivileged user namespaces.
    AppArmorUserNamespaceRestriction,
    /// The process is inside a container that does not permit nesting.
    Container,
    /// WSL1 has no user namespace support at all.
    Wsl1,
    /// Bubblewrap ran and failed for a reason Euler could not attribute.
    Unattributed,
}

impl SandboxFailureCause {
    /// The likely cause, in the user's terms.
    pub const fn description(self) -> &'static str {
        match self {
            Self::BubblewrapMissing => "`bwrap` is not installed at /usr/bin/bwrap or /bin/bwrap",
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
            Self::Unattributed => "Bubblewrap could not create a user namespace",
        }
    }

    /// The host change that would make the sandbox work.
    pub const fn remedy(self) -> &'static str {
        match self {
            Self::BubblewrapMissing => "install it (Debian/Ubuntu: `sudo apt install bubblewrap`)",
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
            Self::Unattributed => {
                "check `sysctl kernel.unprivileged_userns_clone user.max_user_namespaces` and \
run `bwrap --unshare-user --unshare-net --ro-bind / / /bin/true` by hand"
            }
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BubblewrapMissing => "bubblewrap_missing",
            Self::UserNamespacesDisabled => "user_namespaces_disabled",
            Self::AppArmorUserNamespaceRestriction => "apparmor_userns_restriction",
            Self::Container => "container",
            Self::Wsl1 => "wsl1",
            Self::Unattributed => "unattributed",
        }
    }
}

/// The session-start record of which execution boundary agent subprocesses
/// get. It is provenance, not a decision: `Unavailable` fails sandbox-requiring
/// commands closed rather than falling back to the host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxStatus {
    /// Bubblewrap ran a trivial sandboxed command successfully.
    Enforced,
    /// No backend exists for this platform yet; commands run on the host under
    /// the permission decider.
    Host,
    /// Linux with no usable Bubblewrap. Sandbox-requiring commands fail.
    Unavailable {
        reason: SandboxUnavailableReason,
        cause: SandboxFailureCause,
    },
}

impl SandboxStatus {
    /// The value recorded as `sandbox_backend` on `session.start`.
    pub const fn backend_label(self) -> &'static str {
        match self {
            Self::Enforced => SandboxBackend::Bwrap.as_str(),
            Self::Host => SandboxBackend::Host.as_str(),
            Self::Unavailable { .. } => "unavailable",
        }
    }

    pub const fn backend(self) -> Option<SandboxBackend> {
        match self {
            Self::Enforced => Some(SandboxBackend::Bwrap),
            Self::Host => Some(SandboxBackend::Host),
            Self::Unavailable { .. } => None,
        }
    }

    pub const fn reason(self) -> Option<SandboxUnavailableReason> {
        match self {
            Self::Enforced | Self::Host => None,
            Self::Unavailable { reason, .. } => Some(reason),
        }
    }

    /// Classify a probed workspace profile. `None` means no backend was
    /// requested, which today is only the host platforms.
    pub fn from_availability(availability: Option<SandboxAvailability>) -> Self {
        match availability {
            None => Self::Host,
            Some(SandboxAvailability::Enforced(_)) => Self::Enforced,
            Some(SandboxAvailability::Unavailable(reason)) => Self::Unavailable {
                reason,
                cause: match reason {
                    SandboxUnavailableReason::BubblewrapMissing => {
                        SandboxFailureCause::BubblewrapMissing
                    }
                    SandboxUnavailableReason::CannotEnforce => user_namespace_failure_cause(),
                    SandboxUnavailableReason::UnsupportedPlatform
                    | SandboxUnavailableReason::InvalidWorkspace => {
                        SandboxFailureCause::Unattributed
                    }
                },
            },
        }
    }

    /// An operator-facing diagnostic naming the likely cause and the way out.
    pub fn diagnostic(self) -> Option<String> {
        let Self::Unavailable { cause, .. } = self else {
            return None;
        };
        Some(format!(
            "Euler could not start its Linux sandbox: {}.\nTo fix it: {}.\n\
Until then `run_shell` and the `git_*` tools fail closed; there is no automatic \
fallback to host execution. To run without a sandbox deliberately, choose the \
\"Full access (unsandboxed)\" preset (ADR 0021 row D), which a later release adds.",
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

/// A workspace-specific profile that has already been probed. It retains the
/// stable availability result so callers can fail closed without copying raw
/// Bubblewrap diagnostics into tool output or provenance.
#[derive(Clone, Debug)]
pub(crate) struct WorkspaceSandbox {
    workspace: Option<PathBuf>,
    runtime: RuntimeRoots,
    bwrap: Option<PathBuf>,
    availability: SandboxAvailability,
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
        if !cfg!(target_os = "linux") {
            return Self {
                workspace: None,
                runtime,
                bwrap: None,
                availability: SandboxAvailability::Unavailable(
                    SandboxUnavailableReason::UnsupportedPlatform,
                ),
            };
        }
        let Ok(workspace) = canonical_workspace(workspace.as_ref()) else {
            return Self {
                workspace: None,
                runtime,
                bwrap: None,
                availability: SandboxAvailability::Unavailable(
                    SandboxUnavailableReason::InvalidWorkspace,
                ),
            };
        };
        let runtime = runtime.excluding(&workspace);
        let Some(bwrap) = bwrap_path() else {
            return Self {
                workspace: Some(workspace),
                runtime,
                bwrap: None,
                availability: SandboxAvailability::Unavailable(
                    SandboxUnavailableReason::BubblewrapMissing,
                ),
            };
        };
        let availability = probe_profile(&bwrap, &workspace, &runtime, profile);
        Self {
            workspace: Some(workspace),
            runtime,
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
        let bwrap = self
            .bwrap
            .as_deref()
            .ok_or(SandboxUnavailableReason::CannotEnforce)?;
        Ok(bwrap_command(
            bwrap,
            profile,
            workspace,
            &self.runtime,
            env,
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
const SYSTEM_SANDBOX_PATH: &str = "/usr/bin:/bin";
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const SANDBOX_READY_MARKER: &str = "__EULER_SANDBOX_READY__\n";
const SANDBOX_READY_WRAPPER: &str = "printf '__EULER_SANDBOX_READY__\\n'; exec \"$@\"";
/// A toolchain root must be a real subtree, never `/`, a host home, or a
/// single-component directory whose contents are unrelated to a toolchain.
const MIN_RUNTIME_ROOT_COMPONENTS: usize = 2;

/// Toolchain homes the host environment implies, read-only inside the
/// sandbox at their real paths (ADR 0021 row A′).
///
/// `HOME` inside the sandbox is a private tmpfs, so a toolchain installed
/// under the real home is otherwise unreachable and `cargo build` fails with
/// command-not-found. Detection is by environment variable first and
/// conventional location second; the real home itself is never mounted, and
/// the directory that holds these roots is remounted read-only so a write
/// under the real home fails rather than landing in a discarded private copy.
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
            let Some(root) = candidate.and_then(|root| usable_runtime_root(&root, home.as_deref()))
            else {
                continue;
            };
            variables.push((OsString::from(*name), root.clone().into_os_string()));
            roots.push(root);
        }
        for root in SYSTEM_TOOLCHAIN_ROOTS {
            if let Some(root) = usable_runtime_root(Path::new(root), home.as_deref()) {
                roots.push(root);
            }
        }
        let mut runtime = Self {
            roots,
            variables,
            path_entries: Vec::new(),
            home,
        };
        runtime.normalize();
        runtime.path_entries = runtime.host_path_entries_inside_roots(path.as_deref());
        runtime
    }

    /// Drop overlapping and duplicate roots, keeping the outermost of any
    /// nested pair so Bubblewrap never receives two binds for one subtree.
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
        self.variables
            .retain(|(_, value)| self.roots.iter().any(|root| root == Path::new(value)));
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
            .filter(|entry| self.roots.iter().any(|root| entry.starts_with(root)))
            .filter(|entry| seen.insert(entry.clone()))
            .collect()
    }

    /// Remove any root that overlaps the writable workspace: the workspace is
    /// mounted read-write and must not also appear read-only.
    fn excluding(mut self, workspace: &Path) -> Self {
        self.roots
            .retain(|root| !root.starts_with(workspace) && !workspace.starts_with(root));
        self.normalize();
        self.path_entries
            .retain(|entry| self.roots.iter().any(|root| entry.starts_with(root)));
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
        parents.retain(|parent| !RUNTIME_MOUNTS.iter().any(|mount| parent.starts_with(mount)));
        parents
    }

    /// The `PATH` the sandbox exports: mounted toolchain directories first,
    /// then the system runtime.
    fn sandbox_path(&self) -> OsString {
        let mut entries = self.path_entries.clone();
        entries.push(PathBuf::from(SYSTEM_SANDBOX_PATH));
        entries
            .iter()
            .map(|entry| entry.as_os_str().to_os_string())
            .collect::<Vec<_>>()
            .join(OsStr::new(":"))
    }
}

/// Accept a toolchain root only when it is an existing directory that is
/// neither the home itself nor a top-level directory.
fn usable_runtime_root(root: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    if !root.is_dir() || root.components().count() <= MIN_RUNTIME_ROOT_COMPONENTS {
        return None;
    }
    if home.is_some_and(|home| home == root || home.starts_with(&root)) {
        return None;
    }
    if RUNTIME_MOUNTS.iter().any(|mount| root.starts_with(mount)) {
        return None;
    }
    Some(root)
}

/// Probe whether the default profile is actually enforceable for `workspace`.
///
/// The child process gets a private root, can access its workspace, cannot
/// write under the real home, and must enter a network namespace. A failure is
/// intentionally collapsed to a stable public reason: raw Bubblewrap
/// diagnostics may expose host details and are not suitable for model-facing
/// or transcript output.
pub fn probe_workspace_sandbox(workspace: &Path) -> SandboxAvailability {
    WorkspaceSandbox::new(workspace, SandboxProfile::WorkspaceNoNetwork).availability()
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
    if !cfg!(target_os = "linux") {
        return SandboxStatus::Host;
    }
    let Some(bwrap) = bwrap_path() else {
        return SandboxStatus::Unavailable {
            reason: SandboxUnavailableReason::BubblewrapMissing,
            cause: SandboxFailureCause::BubblewrapMissing,
        };
    };
    let mut command = Command::new(bwrap);
    command.env_clear();
    mark_inherited_fds_close_on_exec(&mut command);
    command.args([
        "--unshare-user",
        "--unshare-net",
        "--ro-bind",
        "/",
        "/",
        "/bin/true",
    ]);
    if run_probe_to_completion(command) {
        SandboxStatus::Enforced
    } else {
        SandboxStatus::Unavailable {
            reason: SandboxUnavailableReason::CannotEnforce,
            cause: user_namespace_failure_cause(),
        }
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

fn probe_profile(
    bwrap: &Path,
    workspace: &Path,
    runtime: &RuntimeRoots,
    profile: SandboxProfile,
) -> SandboxAvailability {
    let script = match &runtime.home {
        Some(home) => format!(
            "test -w /workspace && test -d /usr && test ! -w {}",
            shell_quote(home)
        ),
        None => "test -w /workspace && test ! -e /home && test -d /usr".to_owned(),
    };
    let mut command = bwrap_command(
        bwrap,
        profile,
        workspace,
        runtime,
        &[],
        OsStr::new("/bin/sh"),
        ["-c", script.as_str()],
    );
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if run_probe_to_completion(command) {
        SandboxAvailability::Enforced(profile)
    } else {
        SandboxAvailability::Unavailable(SandboxUnavailableReason::CannotEnforce)
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// Run one bounded probe. A probe that outlives its deadline is killed and
/// treated as a failure: the sandbox must never make session start hang.
fn run_probe_to_completion(mut command: Command) -> bool {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Err(_) => return false,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
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

#[allow(clippy::too_many_arguments)]
fn bwrap_command<I, S>(
    bwrap: &Path,
    profile: SandboxProfile,
    workspace: &Path,
    runtime: &RuntimeRoots,
    env: &[(OsString, OsString)],
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
    ]);
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

/// Mount the toolchain roots read-only at their real paths.
///
/// Each holding directory becomes a tmpfs mount point first and is remounted
/// read-only last. Without the remount the holding directory would be an
/// ordinary writable directory in the sandbox's private root tmpfs, so a write
/// under the real home would appear to succeed and be silently discarded.
fn add_runtime_root_mounts(command: &mut Command, runtime: &RuntimeRoots) {
    let parents = runtime.read_only_parents();
    if parents.is_empty() {
        return;
    }
    for parent in &parents {
        command.arg("--tmpfs").arg(parent);
    }
    for root in &runtime.roots {
        command.arg("--ro-bind").arg(root).arg(root);
    }
    for parent in &parents {
        command.arg("--remount-ro").arg(parent);
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
    fn profile_uses_private_root_workspace_bind_and_network_namespace() {
        let temp = tempfile::tempdir().expect("temp workspace");
        let workspace = temp.path().canonicalize().expect("canonical workspace");
        let command = bwrap_command(
            Path::new("/usr/bin/bwrap"),
            SandboxProfile::WorkspaceNoNetwork,
            &workspace,
            &RuntimeRoots::default(),
            &[],
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
    fn detected_toolchain_roots_are_read_only_and_carry_their_variables() {
        let temp = tempfile::tempdir().expect("temp dir");
        let home = temp.path().join("home/example");
        let cargo = home.join(".cargo");
        let rustup = home.join(".rustup");
        std::fs::create_dir_all(cargo.join("bin")).expect("cargo bin");
        std::fs::create_dir_all(&rustup).expect("rustup");
        let path = std::env::join_paths([cargo.join("bin"), PathBuf::from("/usr/local/sbin")])
            .expect("join PATH");
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
        assert!(!sandbox_path.contains("/usr/local/sbin"), "{sandbox_path}");
        // The real home is a read-only mount point, so a write under it
        // fails instead of landing in the sandbox's private root tmpfs.
        assert!(runtime.read_only_parents().contains(&home));
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
        let home = temp.path().join("home/example");
        let cargo = home.join(".cargo");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&cargo).expect("cargo home");
        let runtime = RuntimeRoots::from_environment(
            Some(home.clone()),
            |name| (name == "CARGO_HOME").then(|| cargo.clone().into_os_string()),
            None,
        );
        let home = home.canonicalize().expect("canonical home");
        let cargo = cargo.canonicalize().expect("canonical cargo home");
        let command = bwrap_command(
            Path::new("/usr/bin/bwrap"),
            SandboxProfile::WorkspaceNoNetwork,
            &workspace.canonicalize().expect("canonical workspace"),
            &runtime,
            &[],
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
        assert!(
            diagnostic.contains("Full access (unsandboxed)"),
            "{diagnostic}"
        );
        assert!(SandboxStatus::Host.diagnostic().is_none());
        assert_eq!(SandboxStatus::Host.backend_label(), "host");
        assert_eq!(SandboxStatus::Enforced.backend_label(), "bwrap");
    }

    #[test]
    fn every_failure_cause_names_a_distinct_remedy() {
        for cause in [
            SandboxFailureCause::BubblewrapMissing,
            SandboxFailureCause::UserNamespacesDisabled,
            SandboxFailureCause::AppArmorUserNamespaceRestriction,
            SandboxFailureCause::Container,
            SandboxFailureCause::Wsl1,
            SandboxFailureCause::Unattributed,
        ] {
            assert!(!cause.description().is_empty());
            assert!(!cause.remedy().is_empty());
        }
    }

    #[test]
    fn the_platform_default_is_the_enforced_linux_profile() {
        if cfg!(target_os = "linux") {
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
