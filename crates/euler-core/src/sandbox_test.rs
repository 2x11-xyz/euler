use super::*;
use crate::{ToolError, ToolRegistry};
use serde_json::json;
use std::env;
use std::fs;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvRestore {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

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

    assert_eq!(
        registry.sandbox_availability(),
        Some(SandboxAvailability::Unavailable(
            SandboxUnavailableReason::InvalidWorkspace
        ))
    );
    let error = registry
        .execute("run_shell", &json!({"command": "printf should-not-run"}))
        .expect_err("unavailable sandbox must not fall back to host shell");

    assert!(matches!(
        error,
        ToolError::SandboxUnavailable(SandboxUnavailableReason::InvalidWorkspace)
    ));
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
    let host_fd = fs::File::open(&host_file).expect("open host descriptor");
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
fn clear_close_on_exec(file: &fs::File) {
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is borrowed from a live `File`; these calls only inspect
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
