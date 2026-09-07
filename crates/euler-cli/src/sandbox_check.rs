//! `euler --check-sandbox`: report the execution boundary this host gives
//! agent subprocesses, and say what to do when there is none.
//!
//! The check runs the same two probes a session runs. The backend probe is a
//! trivial sandboxed command, because `bwrap` being installed is not evidence
//! that it works; the workspace probe then confirms the profile Euler actually
//! launches with. Reporting them separately is what distinguishes "this host
//! cannot create a user namespace" from "this workspace cannot be mounted".

use anyhow::{anyhow, Result};
use euler_core::{probe_sandbox_backend, probe_workspace_sandbox, SandboxStatus};
use std::io::Write;

pub(crate) fn check_sandbox(mut stdout: impl Write, mut stderr: impl Write) -> Result<()> {
    let backend = probe_sandbox_backend();
    if backend == SandboxStatus::Host {
        writeln!(
            stdout,
            "sandbox backend: host\n\
Agent subprocesses run directly on this host under the permission decider; \
this platform has no sandbox backend yet."
        )?;
        return Ok(());
    }
    // A session probes its own root, so say which directory this answer is
    // about: the result can differ between workspaces.
    let root = std::env::current_dir()?;
    let status = match backend {
        SandboxStatus::Enforced => probe_workspace_sandbox(&root),
        other => other,
    };
    writeln!(
        stdout,
        "sandbox backend: {}\nworkspace probed: {}",
        status.backend_label(),
        root.display()
    )?;
    let Some(diagnostic) = status.diagnostic() else {
        return Ok(());
    };
    writeln!(stderr, "{diagnostic}")?;
    Err(anyhow!("workspace sandbox is unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_names_the_backend_that_would_run_agent_commands() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = check_sandbox(&mut stdout, &mut stderr);
        let stdout = String::from_utf8(stdout).expect("utf-8 report");

        assert!(stdout.starts_with("sandbox backend: "), "{stdout}");
        if cfg!(target_os = "linux") {
            // Enforced on a userns-capable host, an actionable diagnostic
            // otherwise; never a silent success with no backend.
            if result.is_err() {
                let stderr = String::from_utf8(stderr).expect("utf-8 diagnostic");
                assert!(stderr.contains("To fix it:"), "{stderr}");
                assert!(stderr.contains("fail closed"), "{stderr}");
            }
        } else {
            assert!(result.is_ok());
            assert!(stdout.contains("sandbox backend: host"), "{stdout}");
        }
    }
}
