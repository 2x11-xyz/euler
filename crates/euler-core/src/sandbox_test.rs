use super::*;
use crate::{ToolError, ToolRegistry};
use serde_json::json;
#[cfg(target_os = "linux")]
use std::env;
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::io::{self, Read};
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

    // The sandbox fails closed on every platform — that is what this test
    // guards. Only the *reason* is platform-specific: off Linux the platform
    // check short-circuits before the workspace is ever validated (ADR 0014,
    // `probe_workspace_sandbox`), so the invalid workspace is never reached.
    #[cfg(target_os = "linux")]
    let expected = SandboxUnavailableReason::InvalidWorkspace;
    #[cfg(not(target_os = "linux"))]
    let expected = SandboxUnavailableReason::UnsupportedPlatform;

    assert_eq!(
        registry.sandbox_availability(),
        Some(SandboxAvailability::Unavailable(expected))
    );
    let error = registry
        .execute("run_shell", &json!({"command": "printf should-not-run"}))
        .expect_err("unavailable sandbox must not fall back to host shell");

    assert!(
        matches!(error, ToolError::SandboxUnavailable(reason) if reason == expected),
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
            assert_eq!(shell.exit_code, Some(0));
            assert_eq!(
                fs::read_to_string(workspace.join("sandboxed.txt")).expect("workspace output"),
                "sandboxed"
            );
            let git = git.expect("sandboxed direct git");
            assert_eq!(git.exit_code, Some(0), "git output: {}", git.output);
        }
        SandboxAvailability::Unavailable(reason) => {
            assert!(matches!(
                shell,
                Err(ToolError::SandboxUnavailable(actual)) if actual == reason
            ));
            assert!(matches!(
                git,
                Err(ToolError::SandboxUnavailable(actual)) if actual == reason
            ));
        }
    }
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
                Err(ToolError::SandboxUnavailable(actual)) if actual == reason
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
                Err(ToolError::SandboxUnavailable(actual)) if actual == reason
            ));
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn sandboxed_git_cannot_read_an_inherited_host_descriptor() {
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
    let result = registry.execute("git_status", &json!({}));

    match availability {
        SandboxAvailability::Enforced(_) => {
            let execution = result.expect("sandboxed direct git");
            assert_eq!(execution.exit_code, Some(0), "{}", execution.output);
            assert_eq!(
                fs::read_to_string(workspace.join("fsmonitor-invoked"))
                    .expect("direct git invoked fsmonitor"),
                "invoked"
            );
            assert!(
                !workspace.join("git-fd-leak").exists(),
                "direct git read an inherited host descriptor"
            );
        }
        SandboxAvailability::Unavailable(reason) => {
            assert!(matches!(
                result,
                Err(ToolError::SandboxUnavailable(actual)) if actual == reason
            ));
        }
    }
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
            Err(ToolError::SandboxUnavailable(actual)) if actual == reason
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
                Err(ToolError::SandboxUnavailable(actual)) if actual == reason
            ));
        }
    }
}

#[cfg(target_os = "linux")]
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
