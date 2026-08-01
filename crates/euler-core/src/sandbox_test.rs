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
use std::os::unix::{ffi::OsStrExt, net::UnixListener};
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

#[test]
fn disabled_subprocess_authority_never_falls_back_to_host_execution() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::with_subprocess_sandbox(temp.path(), SubprocessSandbox::Disabled);
    let error = registry
        .execute("run_shell", &json!({"command": "printf bad > escaped"}))
        .expect_err("disabled subprocess authority must fail closed");

    assert!(matches!(error, ToolError::WorkspaceAuthorityRequired));
    assert!(!temp.path().join("escaped").exists());
    assert_eq!(registry.workspace_authority_payload()["mode"], "disabled");
}

#[cfg(target_os = "linux")]
#[test]
fn attached_root_shell_changes_are_confined_and_attributed_per_root() {
    let temp = tempfile::tempdir().expect("temp dir");
    let lightcone = temp.path().join("lightcone");
    let euler = temp.path().join("euler");
    let outside = temp.path().join("outside");
    fs::create_dir(&lightcone).expect("lightcone");
    fs::create_dir(&euler).expect("euler");
    fs::create_dir(&outside).expect("outside");
    let registry = ToolRegistry::with_workspace_authority(
        &lightcone,
        vec![euler.clone()],
        Vec::new(),
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );
    let availability = registry.sandbox_availability().expect("requested sandbox");
    let primary_file = lightcone.join("primary.txt");
    let attached_file = euler.join("attached.txt");
    let outside_file = outside.join("escape.txt");
    let command = format!(
        "printf primary > {}; printf attached > {}; if printf escape > {}; then exit 1; fi",
        shell_quote(&primary_file),
        shell_quote(&attached_file),
        shell_quote(&outside_file),
    );
    let result = registry.execute("run_shell", &json!({"command": command}));

    match availability {
        SandboxAvailability::Enforced(_) => {
            let execution = result.expect("sandboxed shell");
            assert_eq!(execution.exit_code, Some(0));
            assert!(execution
                .file_changes
                .iter()
                .any(|change| change.path == "primary.txt"));
            assert!(execution
                .file_changes
                .iter()
                .any(|change| change.path == "attached.txt"));
            let roots = execution
                .file_changes
                .iter()
                .map(|change| change.workspace_root.clone())
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                roots,
                std::collections::BTreeSet::from([
                    lightcone
                        .canonicalize()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    euler.canonicalize().unwrap().to_string_lossy().into_owned(),
                ])
            );
            assert!(!outside_file.exists());
        }
        SandboxAvailability::Unavailable(reason) => assert!(matches!(
            result,
            Err(ToolError::SandboxUnavailable(actual)) if actual == reason
        )),
    }
}

#[test]
fn structured_attached_paths_use_absolute_selector_and_never_index_grants() {
    let temp = tempfile::tempdir().expect("temp dir");
    let primary = temp.path().join("primary");
    let attached = temp.path().join("attached");
    fs::create_dir(&primary).expect("primary");
    fs::create_dir(&attached).expect("attached");
    let registry = ToolRegistry::with_workspace_authority(
        &primary,
        vec![attached.clone()],
        Vec::new(),
        SubprocessSandbox::Disabled,
    );
    let attached_file = attached.join("new.txt");
    let execution = registry
        .execute(
            "write_file",
            &json!({"path": attached_file, "content": "attached\n"}),
        )
        .expect("prepare attached write");
    let patch = execution.patch.expect("patch");

    assert_eq!(patch.workspace_root, attached.canonicalize().unwrap());
    assert_eq!(patch.path, "new.txt");
    assert_eq!(
        registry.workspace_relative_path(&attached.join("new.txt").to_string_lossy()),
        None,
        "v0 grants have no durable attached-root selector"
    );
    registry.apply_patch(&patch).expect("apply attached write");
    assert_eq!(fs::read_to_string(attached_file).unwrap(), "attached\n");
    let traversal = format!(
        "../{}/traversal.txt",
        attached.file_name().unwrap().to_string_lossy()
    );
    assert!(
        registry
            .execute(
                "write_file",
                &json!({"path": traversal, "content": "must use absolute selector\n"}),
            )
            .is_err(),
        "a relative path must never select an attached root"
    );
    assert!(!attached.join("traversal.txt").exists());
    fs::write(primary.join("inside.txt"), "primary\n").expect("primary file");
    assert!(
        registry
            .execute("read_file", &json!({"path": primary.join("inside.txt")}),)
            .is_err(),
        "primary-root structured paths stay relative"
    );
}

#[test]
fn incomplete_pre_command_snapshot_blocks_shell_before_mutation() {
    let temp = tempfile::tempdir().expect("temp dir");
    for index in 0..=crate::MAX_WORKSPACE_SNAPSHOT_FILES {
        fs::write(temp.path().join(format!("{index:04}.txt")), "x").expect("fixture file");
    }
    let registry = ToolRegistry::new(temp.path());
    let error = registry
        .execute("run_shell", &json!({"command": "printf bad > command-ran"}))
        .expect_err("incomplete observation must stop command");

    assert!(matches!(
        error,
        ToolError::WorkspaceObservationIncomplete {
            phase: "before",
            reason: crate::WorkspaceSnapshotError::FileLimit,
            ..
        }
    ));
    assert!(!temp.path().join("command-ran").exists());
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
    let authority = registry.workspace_authority_payload();
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
            assert_eq!(
                authority["writable_roots"],
                json!([workspace.canonicalize().expect("canonical workspace")])
            );
            assert!(authority["read_only_runtime_roots"]
                .as_array()
                .expect("selected read-only roots")
                .iter()
                .any(|root| root == "/usr"));
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
            assert!(authority["writable_roots"].is_null());
            assert!(authority["read_only_runtime_roots"].is_null());
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
fn final_authority_scan_blocks_new_host_ipc_nodes_before_launch() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let registry = ToolRegistry::with_subprocess_sandbox(
        &workspace,
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );
    if !matches!(
        registry.sandbox_availability(),
        Some(SandboxAvailability::Enforced(_))
    ) {
        return;
    }

    let socket_path = workspace.join("host.sock");
    let listener = UnixListener::bind(&socket_path).expect("host socket");
    let socket_marker = workspace.join("socket-command-ran");
    let error = registry
        .execute(
            "run_shell",
            &json!({"command": format!("printf bad > {}", shell_quote(&socket_marker))}),
        )
        .expect_err("socket must block launch");
    assert!(matches!(
        error,
        ToolError::SandboxUnavailable(SandboxUnavailableReason::UnsafeSpecialNode)
    ));
    assert!(!socket_marker.exists());
    drop(listener);
    fs::remove_file(&socket_path).expect("remove socket");

    let fifo_path = workspace.join("host.fifo");
    let fifo_path_bytes = fifo_path.as_os_str().as_bytes();
    let fifo_path = std::ffi::CString::new(fifo_path_bytes).expect("fifo path");
    // SAFETY: `fifo_path` is a live NUL-terminated pathname and the mode is
    // limited to ordinary owner read/write permissions.
    assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
    let fifo_marker = workspace.join("fifo-command-ran");
    let error = registry
        .execute(
            "run_shell",
            &json!({"command": format!("printf bad > {}", shell_quote(&fifo_marker))}),
        )
        .expect_err("FIFO must block launch");
    assert!(matches!(
        error,
        ToolError::SandboxUnavailable(SandboxUnavailableReason::UnsafeSpecialNode)
    ));
    assert!(!fifo_marker.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn final_authority_scan_rejects_a_replaced_root_on_the_same_mount() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    let original = temp.path().join("original-workspace");
    fs::create_dir(&workspace).expect("workspace");
    let registry = ToolRegistry::with_subprocess_sandbox(
        &workspace,
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );
    if !matches!(
        registry.sandbox_availability(),
        Some(SandboxAvailability::Enforced(_))
    ) {
        return;
    }

    fs::rename(&workspace, &original).expect("move selected root");
    fs::create_dir(&workspace).expect("replace selected root at the same path");
    let marker = workspace.join("command-ran");
    let error = registry
        .execute(
            "run_shell",
            &json!({"command": format!("printf bad > {}", shell_quote(&marker))}),
        )
        .expect_err("replaced root must block launch");

    assert!(matches!(
        error,
        ToolError::SandboxUnavailable(SandboxUnavailableReason::UnsafeMountTopology)
    ));
    assert!(!marker.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn preexisting_host_ipc_node_is_requested_but_never_reported_as_selected() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let _listener = UnixListener::bind(workspace.join("host.sock")).expect("host socket");
    let registry = ToolRegistry::with_subprocess_sandbox(
        &workspace,
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );

    assert_eq!(
        registry.sandbox_availability(),
        Some(SandboxAvailability::Unavailable(
            SandboxUnavailableReason::UnsafeSpecialNode
        ))
    );
    let authority = registry.workspace_authority_payload();
    assert_eq!(authority["enforcement"], "unavailable");
    assert!(authority["writable_roots"].is_null());
    assert!(authority["read_only_runtime_roots"].is_null());
    assert_eq!(
        authority["requested_writable_roots"],
        json!([workspace.canonicalize().expect("canonical workspace")])
    );
}

#[cfg(target_os = "linux")]
#[test]
fn device_filesystem_runtime_root_is_unavailable() {
    let temp = tempfile::tempdir().expect("temp dir");
    let sandbox = WorkspaceSandbox::new(
        temp.path(),
        &[],
        &[PathBuf::from("/dev")],
        SandboxProfile::WorkspaceNoNetwork,
    );

    assert_eq!(
        sandbox.availability(),
        SandboxAvailability::Unavailable(SandboxUnavailableReason::InvalidRuntimeRoots)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn proc_runtime_root_cannot_expose_a_host_descriptor_canary() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    fs::create_dir(&workspace).expect("workspace");
    fs::create_dir(&outside).expect("outside");

    let baseline = ToolRegistry::with_subprocess_sandbox(
        &workspace,
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );
    if !matches!(
        baseline.sandbox_availability(),
        Some(SandboxAvailability::Enforced(_))
    ) {
        return;
    }

    let canary = outside.join("host-fd-canary");
    fs::write(&canary, "host-descriptor-canary").expect("write canary");
    let canary_file = fs::File::open(&canary).expect("open canary");
    let descriptor = canary_file.as_raw_fd();
    let host_proc_root = PathBuf::from(format!("/proc/{}", std::process::id()));
    assert_eq!(
        fs::read_to_string(host_proc_root.join("fd").join(descriptor.to_string()))
            .expect("host proc descriptor canary"),
        "host-descriptor-canary"
    );

    let registry = ToolRegistry::with_workspace_authority(
        &workspace,
        Vec::new(),
        vec![host_proc_root.clone()],
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );
    assert_eq!(
        registry.sandbox_availability(),
        Some(SandboxAvailability::Unavailable(
            SandboxUnavailableReason::UnsafeMountTopology
        ))
    );
    let leak = workspace.join("leaked-canary");
    let error = registry
        .execute(
            "run_shell",
            &json!({
                "command": format!(
                    "cat {}/fd/{descriptor} > {}",
                    shell_quote(&host_proc_root),
                    shell_quote(&leak)
                )
            }),
        )
        .expect_err("proc runtime root must block launch");

    assert!(matches!(
        error,
        ToolError::SandboxUnavailable(SandboxUnavailableReason::UnsafeMountTopology)
    ));
    assert!(!leak.exists());
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
fn sandboxed_git_status_disables_hostile_fsmonitor_configuration() {
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
    let invoked = shell_quote(&workspace.join("fsmonitor-invoked"));
    fs::write(
        &fsmonitor,
        format!(
            "#!/bin/sh\nprintf invoked > {invoked}\n\
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
        .arg("config")
        .arg("core.fsmonitor")
        .arg(&fsmonitor)
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
            assert!(
                !workspace.join("fsmonitor-invoked").exists(),
                "direct git must override repository fsmonitor configuration"
            );
            assert!(execution.file_changes.is_empty());
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
fn sandboxed_git_diff_disables_external_diff_and_textconv_helpers() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    let initialized = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&workspace)
        .status()
        .expect("git available");
    assert!(initialized.success(), "initialize workspace repository");
    std::process::Command::new("git")
        .args(["config", "user.email", "euler-test@example.invalid"])
        .current_dir(&workspace)
        .status()
        .expect("configure email");
    std::process::Command::new("git")
        .args(["config", "user.name", "Euler Test"])
        .current_dir(&workspace)
        .status()
        .expect("configure name");
    fs::write(
        workspace.join(".gitattributes"),
        "tracked.txt diff=hostile\n",
    )
    .expect("attributes");
    fs::write(workspace.join("tracked.txt"), "before\n").expect("tracked file");
    let added = std::process::Command::new("git")
        .args(["add", ".gitattributes", "tracked.txt"])
        .current_dir(&workspace)
        .status()
        .expect("git add");
    assert!(added.success(), "git add fixture");
    let committed = std::process::Command::new("git")
        .args(["commit", "--quiet", "-m", "fixture"])
        .current_dir(&workspace)
        .status()
        .expect("git commit");
    assert!(committed.success(), "git commit fixture");

    let external_marker = workspace.join("external-diff-invoked");
    let textconv_marker = workspace.join("textconv-invoked");
    let external = workspace.join("external-diff");
    let textconv = workspace.join("textconv");
    write_executable_marker_script(&external, &external_marker);
    write_executable_marker_script(&textconv, &textconv_marker);
    for (key, value) in [
        ("diff.external", external.as_path()),
        ("diff.hostile.textconv", textconv.as_path()),
    ] {
        let configured = std::process::Command::new("git")
            .arg("config")
            .arg(key)
            .arg(value)
            .current_dir(&workspace)
            .status()
            .expect("configure hostile diff helper");
        assert!(configured.success(), "configure {key}");
    }
    fs::write(workspace.join("tracked.txt"), "after\n").expect("modify tracked file");

    let registry = ToolRegistry::with_subprocess_sandbox(
        &workspace,
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );
    let availability = registry
        .sandbox_availability()
        .expect("sandbox was requested");
    let result = registry.execute("git_diff", &json!({}));

    match availability {
        SandboxAvailability::Enforced(_) => {
            let execution = result.expect("sandboxed direct git");
            assert_eq!(execution.exit_code, Some(0), "{}", execution.output);
            assert!(execution.output.contains("-before"));
            assert!(execution.output.contains("+after"));
            assert!(!external_marker.exists(), "external diff ran");
            assert!(!textconv_marker.exists(), "textconv ran");
            assert!(execution.file_changes.is_empty());
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
fn sandboxed_git_diff_records_and_fails_on_filter_side_effects() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    let initialized = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&workspace)
        .status()
        .expect("git available");
    assert!(initialized.success(), "initialize workspace repository");
    for (key, value) in [
        ("user.email", "euler-test@example.invalid"),
        ("user.name", "Euler Test"),
    ] {
        let configured = std::process::Command::new("git")
            .args(["config", key, value])
            .current_dir(&workspace)
            .status()
            .expect("configure repository");
        assert!(configured.success(), "configure {key}");
    }
    fs::write(
        workspace.join(".gitattributes"),
        "tracked.txt filter=hostile\n",
    )
    .expect("attributes");
    fs::write(workspace.join("tracked.txt"), "before\n").expect("tracked file");
    let added = std::process::Command::new("git")
        .args(["add", ".gitattributes", "tracked.txt"])
        .current_dir(&workspace)
        .status()
        .expect("git add");
    assert!(added.success(), "git add fixture");
    let committed = std::process::Command::new("git")
        .args(["commit", "--quiet", "-m", "fixture"])
        .current_dir(&workspace)
        .status()
        .expect("git commit");
    assert!(committed.success(), "git commit fixture");

    let marker = workspace.join("filter-invoked");
    let filter = workspace.join("filter-helper");
    fs::write(
        &filter,
        format!(
            "#!/bin/sh\nprintf invoked > {}\ncat\n",
            shell_quote(&marker)
        ),
    )
    .expect("write filter helper");
    let mut permissions = fs::metadata(&filter)
        .expect("filter metadata")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&filter, permissions).expect("make filter executable");
    let configured = std::process::Command::new("git")
        .arg("config")
        .arg("filter.hostile.clean")
        .arg(&filter)
        .current_dir(&workspace)
        .status()
        .expect("configure hostile clean filter");
    assert!(configured.success(), "configure clean filter");
    fs::write(workspace.join("tracked.txt"), "after\n").expect("modify tracked file");

    let registry = ToolRegistry::with_subprocess_sandbox(
        &workspace,
        SubprocessSandbox::Enforce(SandboxProfile::WorkspaceNoNetwork),
    );
    let availability = registry
        .sandbox_availability()
        .expect("sandbox was requested");
    let result = registry.execute("git_diff", &json!({}));

    match availability {
        SandboxAvailability::Enforced(_) => {
            let execution = result.expect("sandboxed direct git");
            assert_eq!(execution.exit_code, Some(-1), "{}", execution.output);
            assert!(execution.output.contains("observed workspace mutation"));
            assert_eq!(
                fs::read_to_string(&marker).expect("filter marker"),
                "invoked"
            );
            assert!(execution
                .file_changes
                .iter()
                .any(|change| change.path == "filter-invoked" && change.action == "add"));
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
fn write_executable_marker_script(path: &std::path::Path, marker: &std::path::Path) {
    fs::write(
        path,
        format!(
            "#!/bin/sh\nprintf invoked > {}\nif test -f \"$1\"; then cat \"$1\"; fi\n",
            shell_quote(marker)
        ),
    )
    .expect("write helper script");
    let mut permissions = fs::metadata(path).expect("helper metadata").permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).expect("make helper executable");
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
