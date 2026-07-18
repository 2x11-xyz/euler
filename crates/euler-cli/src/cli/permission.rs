use euler_core::permissions::{DeciderVerdict, PermissionRequest, PermissionRequestBatch};
use euler_core::PermissionDecider;
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Default)]
pub(crate) struct EnvArgs {
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) auth_file: Option<PathBuf>,
}

pub(crate) struct CliDecider;

impl PermissionDecider for CliDecider {
    fn decide(&mut self, request: &PermissionRequest) -> DeciderVerdict {
        eprint!(
            "permission: allow {} for {}? [y/N] ",
            request.capability.as_str(),
            request.reason
        );
        let _ = io::stderr().flush();
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer).is_ok()
            && matches!(answer.trim(), "y" | "Y" | "yes" | "YES")
        {
            DeciderVerdict::Allow
        } else {
            DeciderVerdict::Deny
        }
    }

    fn decide_batch(&mut self, batch: &PermissionRequestBatch) -> DeciderVerdict {
        let capabilities = batch
            .capabilities()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        eprint!(
            "permission: allow {capabilities} for {}? [y/N] ",
            batch.operation()
        );
        let _ = io::stderr().flush();
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer).is_ok()
            && matches!(answer.trim(), "y" | "Y" | "yes" | "YES")
        {
            DeciderVerdict::Allow
        } else {
            DeciderVerdict::Deny
        }
    }
}
