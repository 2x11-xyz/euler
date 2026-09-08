use super::*;
use crate::{ToolError, ToolRegistry};
use serde_json::json;
#[cfg(target_os = "linux")]
use std::env;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::fs;
#[cfg(target_os = "linux")]
use std::io::{self, Read};
#[cfg(target_os = "macos")]
use std::net::TcpListener;
#[cfg(target_os = "linux")]
use std::net::{TcpListener, TcpStream};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "linux")]
use std::sync::Mutex;
#[cfg(target_os = "linux")]
use std::time::Duration;

// The sandbox is Linux-only (ADR 0014), so every fixture below it is too:
// off Linux these are unreachable and `-D dead-code` rejects them.
#[cfg(target_os = "linux")]
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[cfg(target_os = "linux")]
struct EnvRestore {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

#[cfg(target_os = "linux")]
impl EnvRestore {
    fn capture(names: &[&'static str]) -> Self {
        Self {
            saved: names
                .iter()
                .map(|name| (*name, env::var_os(name)))
                .collect(),
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => env::set_var(*name, value),
                None => env::remove_var(*name),
            }
        }
    }
}

#[test]
fn requested_but_invalid_profile_fails_closed_before_shell_execution() {
    let temp = tempfile::tempdir().expect("temp dir");
    let missing_workspace = temp.path().join("missing");
    let registry = ToolRegistry::with_subprocess_sandbox(
        &missing_workspace,
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );

    // The sandbox fails closed on every platform. Implemented backends reach
    // workspace validation; unsupported platforms stop before it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let expected = SandboxUnavailableReason::InvalidWorkspace;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let expected = SandboxUnavailableReason::UnsupportedPlatform;

    assert_eq!(
        registry.sandbox_availability(),
        Some(SandboxAvailability::Unavailable(expected))
    );
    let error = registry
        .execute("run_shell", &json!({"command": "printf should-not-run"}))
        .expect_err("unavailable sandbox must not fall back to host shell");

    assert!(
        matches!(error, ToolError::SandboxUnavailable { reason, .. } if reason == expected),
        "run_shell must refuse with the sandbox's own reason, got: {error:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn selected_workspace_profile_routes_shell_and_git_or_fails_closed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&outside).expect("outside");
    let secret = outside.join("secret");
    fs::write(&secret, "host-only").expect("plant secret");
    let initialized = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&workspace)
        .status()
        .expect("git available for git_status tool");
    assert!(initialized.success(), "initialize workspace repository");

    let registry = ToolRegistry::with_subprocess_sandbox(
        &workspace,
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );
    let availability = registry
        .sandbox_availability()
        .expect("sandbox was requested");
    let secret = shell_quote(&secret);
    let shell = registry.execute(
        "run_shell",
        &json!({
            "command": format!(
                "test ! -e /home; test ! -e {secret}; printf sandboxed > sandboxed.txt"
            )
        }),
    );
    let git = registry.execute("git_status", &json!({}));

    match availability {
        SandboxAvailability::Enforced(_) => {
            let shell = shell.expect("sandboxed shell");
            assert_eq!(shell.sandbox_backend, Some(SandboxBackend::Bwrap));
            assert_eq!(shell.exit_code, Some(0));
            assert_eq!(
                fs::read_to_string(workspace.join("sandboxed.txt")).expect("workspace output"),
                "sandboxed"
            );
            let git = git.expect("sandboxed direct git");
            assert_eq!(git.sandbox_backend, Some(SandboxBackend::Bwrap));
            assert_eq!(git.exit_code, Some(0), "git output: {}", git.output);
        }
        SandboxAvailability::Unavailable(reason) => {
            assert!(matches!(
                shell,
                Err(ToolError::SandboxUnavailable { reason: actual, .. }) if actual == reason
            ));
            assert!(matches!(
                git,
                Err(ToolError::SandboxUnavailable { reason: actual, .. }) if actual == reason
            ));
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_seatbelt_confines_shell_and_git_or_is_explicitly_nested() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&outside).expect("outside");
    let initialized = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&workspace)
        .status()
        .expect("git available");
    assert!(initialized.success(), "initialize workspace repository");
    let git_config_before = fs::read(workspace.join(".git/config")).expect("read Git config");

    let registry = ToolRegistry::with_subprocess_sandbox(
        &workspace,
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );
    let availability = registry
        .sandbox_availability()
        .expect("sandbox was requested");
    if let SandboxAvailability::Unavailable(reason) = availability {
        assert_eq!(reason, SandboxUnavailableReason::CannotEnforce);
        assert!(
            running_inside_nested_seatbelt(&workspace),
            "macOS CI must enforce Seatbelt; only an explicit nested-Seatbelt refusal may skip"
        );
        return;
    }

    let outside_file = outside.join("escaped");
    let normal = registry
        .execute(
            "run_shell",
            &json!({"command": "printf confined > ordinary.txt"}),
        )
        .expect("ordinary workspace write");
    assert_eq!(normal.exit_code, Some(0), "{}", normal.output);
    assert_eq!(normal.sandbox_backend, Some(SandboxBackend::Seatbelt));
    assert_eq!(
        fs::read_to_string(workspace.join("ordinary.txt")).expect("workspace output"),
        "confined"
    );
    let ordinary_mutation = registry
        .execute(
            "run_shell",
            &json!({"command": "mv ordinary.txt renamed.txt && rm renamed.txt"}),
        )
        .expect("ordinary workspace rename and unlink");
    assert_eq!(
        ordinary_mutation.exit_code,
        Some(0),
        "{}",
        ordinary_mutation.output
    );
    assert!(!workspace.join("ordinary.txt").exists());
    assert!(!workspace.join("renamed.txt").exists());

    for command in [
        "printf forbidden > .git/euler-seatbelt-denied".to_owned(),
        "mv .git .git-moved".to_owned(),
        "ln .git/config git-config-link && printf forbidden > git-config-link".to_owned(),
        format!("printf escaped > {}", shell_quote(&outside_file)),
    ] {
        let denied = registry
            .execute("run_shell", &json!({"command": &command}))
            .expect("Seatbelt denial is a completed shell result");
        assert_ne!(denied.exit_code, Some(0), "{command}: {}", denied.output);
        assert_eq!(denied.sandbox_backend, Some(SandboxBackend::Seatbelt));
    }
    assert!(workspace.join(".git").is_dir(), "Git metadata was renamed");
    assert!(!workspace.join(".git/euler-seatbelt-denied").exists());
    assert!(!workspace.join("git-config-link").exists());
    assert_eq!(
        fs::read(workspace.join(".git/config")).expect("read protected Git config"),
        git_config_before
    );
    assert!(!outside_file.exists());

    let git = registry
        .execute("git_status", &json!({}))
        .expect("direct Git read");
    assert_eq!(git.exit_code, Some(0), "{}", git.output);
    assert_eq!(git.sandbox_backend, Some(SandboxBackend::Seatbelt));

    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    let port = listener.local_addr().expect("listener address").port();
    let network = registry
        .execute(
            "run_shell",
            &json!({"command": format!("/usr/bin/nc -z -w 1 127.0.0.1 {port}")}),
        )
        .expect("network denial is a completed shell result");
    assert_ne!(network.exit_code, Some(0), "{}", network.output);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_seatbelt_blocks_first_time_git_metadata_creation() {
    let workspace = tempfile::tempdir().expect("temp workspace");
    let registry = ToolRegistry::with_subprocess_sandbox(
        workspace.path(),
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );
    if let Some(SandboxAvailability::Unavailable(reason)) = registry.sandbox_availability() {
        assert_eq!(reason, SandboxUnavailableReason::CannotEnforce);
        assert!(running_inside_nested_seatbelt(workspace.path()));
        return;
    }

    let denied = registry
        .execute("run_shell", &json!({"command": "mkdir .git"}))
        .expect("Seatbelt denial is a completed shell result");
    assert_ne!(denied.exit_code, Some(0), "{}", denied.output);
    assert!(!workspace.path().join(".git").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_seatbelt_fails_closed_for_a_symlinked_git_entry() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().expect("temp workspace");
    fs::create_dir(workspace.path().join("metadata")).expect("metadata target");
    symlink("metadata", workspace.path().join(".git")).expect("symlinked .git");
    let registry = ToolRegistry::with_subprocess_sandbox(
        workspace.path(),
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );

    assert_eq!(
        registry.sandbox_availability(),
        Some(SandboxAvailability::Unavailable(
            SandboxUnavailableReason::GitMetadataSymlink
        ))
    );
    let error = registry
        .execute("run_shell", &json!({"command": "printf forbidden"}))
        .expect_err("symlinked .git must fail closed");
    assert!(matches!(
        error,
        ToolError::SandboxUnavailable {
            reason: SandboxUnavailableReason::GitMetadataSymlink,
            ..
        }
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_seatbelt_fails_closed_for_an_in_workspace_gitdir_target() {
    let workspace = tempfile::tempdir().expect("temp workspace");
    let metadata = workspace.path().join("metadata");
    fs::create_dir(&metadata).expect("metadata target");
    fs::write(workspace.path().join(".git"), "gitdir: metadata\n").expect("gitdir pointer");
    let registry = ToolRegistry::with_subprocess_sandbox(
        workspace.path(),
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );
    assert_eq!(
        registry.sandbox_availability(),
        Some(SandboxAvailability::Unavailable(
            SandboxUnavailableReason::CannotEnforce
        ))
    );
    let error = registry
        .execute("run_shell", &json!({"command": "printf forbidden"}))
        .expect_err("in-workspace gitdir target must fail closed");
    assert!(matches!(
        error,
        ToolError::SandboxUnavailable {
            reason: SandboxUnavailableReason::CannotEnforce,
            ..
        }
    ));
}

#[cfg(target_os = "macos")]
fn running_inside_nested_seatbelt(workspace: &std::path::Path) -> bool {
    let workspace = workspace.canonicalize().expect("canonical workspace");
    let scratch = tempfile::tempdir().expect("Seatbelt scratch");
    let scratch_path = scratch.path().canonicalize().expect("canonical scratch");
    for directory in ["home", "cache", "tmp"] {
        fs::create_dir(scratch_path.join(directory)).expect("scratch directory");
    }
    let git_metadata = workspace.join(".git");
    let git_metadata_resolved = git_metadata
        .canonicalize()
        .unwrap_or_else(|_| git_metadata.clone());
    let output = seatbelt_command(
        SeatbeltCommand {
            executable: std::path::Path::new(SEATBELT_PATH),
            scratch: &scratch_path,
            git_metadata: &git_metadata,
            git_metadata_resolved: &git_metadata_resolved,
        },
        SandboxLaunch {
            profile: SandboxProfile::WorkspaceNoNetwork,
            workspace: &workspace,
            runtime: &RuntimeRoots::default(),
            env: &[],
        },
        std::ffi::OsStr::new("/usr/bin/true"),
        std::iter::empty::<&str>(),
    )
    .output()
    .expect("run direct Seatbelt canary");
    !output.status.success()
        && String::from_utf8_lossy(&output.stderr)
            .contains("sandbox-exec: sandbox_apply: Operation not permitted")
}

#[cfg(target_os = "linux")]
#[test]
fn sandboxed_shell_uses_only_the_profile_environment() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let _env_restore = EnvRestore::capture(&["EULER_SANDBOX_VISIBLE"]);
    env::set_var("EULER_SANDBOX_VISIBLE", "host-visible");
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::with_subprocess_sandbox(
        temp.path(),
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );
    let availability = registry
        .sandbox_availability()
        .expect("sandbox was requested");
    let result = registry.execute(
        "run_shell",
        &json!({
            "command": "printf '%s|%s|%s' \"$EULER_SANDBOX_VISIBLE\" \"$HOME\" \"$TMPDIR\""
        }),
    );

    match availability {
        SandboxAvailability::Enforced(_) => {
            let execution = result.expect("sandboxed shell");
            assert_eq!(execution.exit_code, Some(0));
            assert!(execution.output.contains("|/tmp/home|/tmp"));
            assert!(!execution.output.contains("host-visible"));
        }
        SandboxAvailability::Unavailable(reason) => {
            assert!(matches!(
                result,
                Err(ToolError::SandboxUnavailable { reason: actual, .. }) if actual == reason
            ));
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn sandboxed_shell_cannot_read_an_inherited_host_descriptor() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&outside).expect("outside");
    let host_file = outside.join("host-fd");
    fs::write(&host_file, "inherited-host-descriptor").expect("host file");
    let opened_host_fd = fs::File::open(&host_file).expect("open host descriptor");
    let host_fd = duplicate_at_or_above(&opened_host_fd, 100);
    clear_close_on_exec(&host_fd);

    let registry = ToolRegistry::with_subprocess_sandbox(
        &workspace,
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );
    let availability = registry
        .sandbox_availability()
        .expect("sandbox was requested");
    let fd = host_fd.as_raw_fd();
    let result = registry.execute(
        "run_shell",
        &json!({
            "command": format!(
                "if test -r /proc/self/fd/{fd} && grep -qx inherited-host-descriptor /proc/self/fd/{fd}; then exit 1; fi"
            )
        }),
    );

    match availability {
        SandboxAvailability::Enforced(_) => {
            let execution = result.expect("sandboxed shell");
            assert_eq!(
                execution.exit_code,
                Some(0),
                "sandbox read an inherited host descriptor: {}",
                execution.output
            );
        }
        SandboxAvailability::Unavailable(reason) => {
            assert!(matches!(
                result,
                Err(ToolError::SandboxUnavailable { reason: actual, .. }) if actual == reason
            ));
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn sandboxed_agent_git_cannot_read_an_inherited_host_descriptor() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&outside).expect("outside");
    let initialized = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&workspace)
        .status()
        .expect("git available for git_status tool");
    assert!(initialized.success(), "initialize workspace repository");

    let host_file = outside.join("host-fd");
    fs::write(&host_file, "inherited-host-descriptor").expect("host file");
    let opened_host_fd = fs::File::open(&host_file).expect("open host descriptor");
    let host_fd = duplicate_at_or_above(&opened_host_fd, 100);
    clear_close_on_exec(&host_fd);
    let fd = host_fd.as_raw_fd();

    let fsmonitor = workspace.join("fsmonitor");
    fs::write(
        &fsmonitor,
        format!(
            "#!/bin/sh\nprintf invoked > /workspace/fsmonitor-invoked\n\
             if test -r /proc/self/fd/{fd}; then cat /proc/self/fd/{fd} > /workspace/git-fd-leak; fi\n\
             printf 'version 2\\n'\nprintf 'token\\n'\n"
        ),
    )
    .expect("write fsmonitor hook");
    let mut permissions = fs::metadata(&fsmonitor)
        .expect("fsmonitor metadata")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&fsmonitor, permissions).expect("make fsmonitor executable");
    let configured = std::process::Command::new("git")
        .args(["config", "core.fsmonitor", "/workspace/fsmonitor"])
        .current_dir(&workspace)
        .status()
        .expect("configure fsmonitor hook");
    assert!(configured.success(), "configure fsmonitor hook");

    let registry = ToolRegistry::with_subprocess_sandbox(
        &workspace,
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );
    let availability = registry
        .sandbox_availability()
        .expect("sandbox was requested");
    // Git the agent runs itself, not Euler's `git_status`: ADR 0021 row G
    // neutralizes repository-selected helpers for Euler's own invocations, so
    // the sandbox is what has to hold for an agent-run one. That makes this
    // the case where a repository-controlled program really does execute
    // inside git, which is what an inherited descriptor would leak through.
    let result = registry.execute("run_shell", &json!({"command": "git status --short"}));

    match availability {
        SandboxAvailability::Enforced(_) => {
            let execution = result.expect("sandboxed agent git");
            assert_eq!(execution.exit_code, Some(0), "{}", execution.output);
            assert_eq!(
                fs::read_to_string(workspace.join("fsmonitor-invoked"))
                    .expect("agent-run git invoked fsmonitor"),
                "invoked"
            );
            assert!(
                !workspace.join("git-fd-leak").exists(),
                "agent-run git read an inherited host descriptor"
            );
        }
        SandboxAvailability::Unavailable(reason) => {
            assert!(matches!(
                result,
                Err(ToolError::SandboxUnavailable { reason: actual, .. }) if actual == reason
            ));
        }
    }
}

/// The companion to the test above: Euler's own `git_status` must not run the
/// same repository-selected helper at all (ADR 0021 row G).
#[cfg(target_os = "linux")]
#[test]
fn sandboxed_direct_git_does_not_run_a_repository_selected_fsmonitor() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    let initialized = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&workspace)
        .status()
        .expect("git available for git_status tool");
    assert!(initialized.success(), "initialize workspace repository");
    let fsmonitor = workspace.join("fsmonitor");
    fs::write(
        &fsmonitor,
        "#!/bin/sh\nprintf invoked > /workspace/fsmonitor-invoked\n\
         printf 'version 2\\n'\nprintf 'token\\n'\n",
    )
    .expect("write fsmonitor hook");
    let mut permissions = fs::metadata(&fsmonitor)
        .expect("fsmonitor metadata")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&fsmonitor, permissions).expect("make fsmonitor executable");
    let configured = std::process::Command::new("git")
        .args(["config", "core.fsmonitor", "/workspace/fsmonitor"])
        .current_dir(&workspace)
        .status()
        .expect("configure fsmonitor hook");
    assert!(configured.success(), "configure fsmonitor hook");

    let registry = ToolRegistry::with_subprocess_sandbox(
        &workspace,
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );
    if !registry
        .sandbox_availability()
        .expect("sandbox was requested")
        .is_enforced()
    {
        return;
    }
    let execution = registry
        .execute("git_status", &json!({}))
        .expect("sandboxed direct git");

    assert_eq!(execution.exit_code, Some(0), "{}", execution.output);
    assert!(
        !workspace.join("fsmonitor-invoked").exists(),
        "direct git ran a repository-selected fsmonitor helper"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn sandboxed_shell_cannot_use_an_inherited_host_socket() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    let registry = ToolRegistry::with_subprocess_sandbox(
        &workspace,
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );
    let availability = registry
        .sandbox_availability()
        .expect("sandbox was requested");
    if let SandboxAvailability::Unavailable(reason) = availability {
        let result = registry.execute("run_shell", &json!({"command": "printf should-not-run"}));
        assert!(matches!(
            result,
            Err(ToolError::SandboxUnavailable { reason: actual, .. }) if actual == reason
        ));
        return;
    }

    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        // Some hermetic test runners prohibit host network sockets entirely.
        // The enforced profile is still valid there, but this regression needs
        // a host socket to establish its canary and cannot run.
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("host listener: {error}"),
    };
    let client = match TcpStream::connect(listener.local_addr().expect("listener address")) {
        Ok(client) => client,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("host client: {error}"),
    };
    let (mut peer, _) = listener.accept().expect("accept host client");
    peer.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("read timeout");
    clear_close_on_exec(&client);

    let fd = client.as_raw_fd();
    let result = registry.execute(
        "run_shell",
        &json!({
            "command": format!(
                "if test -e /proc/self/fd/{fd}; then printf inherited-host-socket >&{fd} 2>/dev/null || true; fi"
            )
        }),
    );

    let execution = result.expect("sandboxed shell");
    assert_eq!(execution.exit_code, Some(0), "{}", execution.output);
    let mut received = [0_u8; 64];
    match peer.read(&mut received) {
        Ok(0) => {}
        Ok(size) => panic!(
            "sandbox wrote through an inherited host socket: {:?}",
            String::from_utf8_lossy(&received[..size])
        ),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) => {}
        Err(error) => panic!("read host socket: {error}"),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn sandboxed_shell_timeout_kills_the_bubblewrap_process_group() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::with_subprocess_sandbox(
        temp.path(),
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );
    let Some(availability) = registry.sandbox_availability() else {
        panic!("sandbox was requested");
    };
    let result = registry.execute(
        "run_shell",
        &json!({
            "command": "echo sandbox-phase-one; sleep 30 & sleep 30; echo sandbox-phase-two",
            "timeout_ms": 200
        }),
    );

    match availability {
        SandboxAvailability::Enforced(_) => {
            let execution = result.expect("timeout is a tool result");
            assert_eq!(execution.exit_code, Some(-1));
            assert!(execution.output.contains("sandbox-phase-one"));
            assert!(!execution.output.contains("sandbox-phase-two"));
        }
        SandboxAvailability::Unavailable(reason) => {
            assert!(matches!(
                result,
                Err(ToolError::SandboxUnavailable { reason: actual, .. }) if actual == reason
            ));
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn shell_quote(path: &std::path::Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[cfg(target_os = "linux")]
fn clear_close_on_exec(descriptor: &impl AsRawFd) {
    let fd = descriptor.as_raw_fd();
    // SAFETY: `fd` is borrowed from a live descriptor; these calls only inspect
    // and clear its close-on-exec bit for this regression probe.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        assert!(flags >= 0, "read descriptor flags");
        assert_eq!(
            libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC),
            0,
            "clear close-on-exec"
        );
    }
}

#[cfg(target_os = "linux")]
fn duplicate_at_or_above(descriptor: &impl AsRawFd, minimum_fd: libc::c_int) -> OwnedFd {
    // SAFETY: `descriptor` is live, and `F_DUPFD` returns a new owned file
    // descriptor at or above `minimum_fd` on success.
    let duplicated = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_DUPFD, minimum_fd) };
    assert!(duplicated >= minimum_fd, "duplicate descriptor at high fd");
    // SAFETY: `F_DUPFD` returned a fresh owned descriptor.
    unsafe { OwnedFd::from_raw_fd(duplicated) }
}
