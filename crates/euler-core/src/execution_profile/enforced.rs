//! The boundaries this build can enforce, and the subprocess sandbox each
//! one becomes. A profile with the sandbox on maps to one of two shapes:
//! the `locked` boundary (no network, private `/tmp` and caches) or the
//! `workspace` boundary (network, host `/tmp`, writable caches), which also
//! honors `writable_paths` and `credentials`. Any other mix is refused with
//! the first field this build cannot deliver.

use super::{
    locked, workspace, ApprovalsSetting, CredentialsSetting, NetworkSetting, ProfileFields,
    SandboxSetting,
};
use crate::sandbox::{SandboxProfile, SubprocessSandbox, WorkspaceNetworkAccess};
use std::path::{Path, PathBuf};

/// The subprocess sandbox for `fields`, or the first field this build cannot
/// enforce on this host.
pub(super) fn subprocess_sandbox(
    fields: &ProfileFields,
) -> Result<SubprocessSandbox, &'static str> {
    let backend_enforced = match fields.sandbox {
        SandboxSetting::On => true,
        SandboxSetting::Bubblewrap => cfg!(target_os = "linux"),
        SandboxSetting::Seatbelt => cfg!(target_os = "macos"),
        SandboxSetting::Off => false,
    };
    if !backend_enforced {
        return Err("sandbox");
    }
    // `never` drops the sensitive-path asks, which is the `full` preset's
    // rule; escalation ships in a later step.
    if fields.approvals == ApprovalsSetting::Never {
        return Err("approvals");
    }
    let (shape, other_shape) = if fields.network == NetworkSetting::On {
        (workspace::fields(), locked::fields())
    } else {
        (locked::fields(), workspace::fields())
    };
    let fixed = |shape: &ProfileFields| {
        [
            ("tmp", fields.tmp == shape.tmp),
            ("caches", fields.caches == shape.caches),
            ("escalation", fields.escalation == shape.escalation),
            (
                "background_jobs",
                fields.background_jobs == shape.background_jobs,
            ),
        ]
    };
    if let Some(index) = fixed(&shape).iter().position(|(_, enforced)| !enforced) {
        // A field the other boundary would deliver means `network` picked
        // the wrong boundary (`{"extends": "locked", "network": "on"}`), so
        // name the field the user changed.
        return Err(if fixed(&other_shape)[index].1 {
            "network"
        } else {
            fixed(&shape)[index].0
        });
    }
    if fields.network == NetworkSetting::Off {
        if !fields.writable_paths.is_empty() {
            return Err("writable_paths");
        }
        if fields.credentials != CredentialsSetting::Private {
            return Err("credentials");
        }
        // Today's boundary, including host execution on a platform with no
        // backend.
        return Ok(SubprocessSandbox::default());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let writable_paths = fields
        .writable_paths
        .iter()
        .map(|path| host_writable_path(path, home.as_deref()))
        .collect::<Option<Vec<_>>>()
        .ok_or("writable_paths")?;
    Ok(SubprocessSandbox::Enforce(
        SandboxProfile::WorkspaceNetwork(WorkspaceNetworkAccess {
            writable_paths,
            pass_credentials: fields.credentials == CredentialsSetting::Pass,
        }),
    ))
}

/// A `writable_paths` entry as an existing canonical host directory or
/// file. `~/` expands to the user's home. A missing path is not
/// enforceable: Bubblewrap cannot bind it and Seatbelt would allow writes
/// to a path that later appears under a symlink.
fn host_writable_path(path: &str, home: Option<&Path>) -> Option<PathBuf> {
    let path = match path.strip_prefix("~/") {
        Some(rest) => home?.join(rest),
        None => PathBuf::from(path),
    };
    path.canonicalize().ok()
}
