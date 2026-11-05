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
        eprint!("{}", prompt_text(request));
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
        let notes = batch
            .ask_rules()
            .into_iter()
            .map(|rule| format!(" ({})", printable(&rule.ask_note())))
            .collect::<String>();
        eprint!(
            "permission: allow {capabilities} for {}{notes}? [y/N] ",
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

/// Text from the model or a profile, with no terminal control sequences.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

/// The question the plain CLI decider asks for one request.
fn prompt_text(request: &PermissionRequest) -> String {
    if let (Some(reason), Some(command)) = (&request.escalation, &request.command) {
        return format!(
            "permission: run `{}` outside the sandbox ({})? [y/N] ",
            printable(command),
            printable(reason)
        );
    }
    let capability = request.capability.as_str();
    match request.ask_rule_note() {
        Some(note) => format!(
            "permission: allow {capability} for {} ({})? [y/N] ",
            request.reason,
            printable(&note)
        ),
        None => format!(
            "permission: allow {capability} for {}? [y/N] ",
            request.reason
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::prompt_text;
    use euler_core::execution_profile::{
        PermissionRule, RuleLayerLabel, RuleMatch, RuleState, RuleSurface,
    };
    use euler_core::permissions::PermissionRequest;
    use euler_sdk::Capability;

    #[test]
    fn prompt_text_names_the_rule_that_asks() {
        let request = PermissionRequest::new(Capability::ShellExec, "tool run_shell")
            .with_command("git push origin main")
            .with_ask_rule(RuleMatch {
                surface: RuleSurface::Shell,
                layer: RuleLayerLabel::Profile,
                index: 0,
                rule: PermissionRule {
                    pattern: "git push *".to_owned(),
                    state: RuleState::Ask,
                    access: None,
                    reason: Some("Pushes need review.".to_owned()),
                },
            });
        assert_eq!(
            prompt_text(&request),
            "permission: allow shell-exec for tool run_shell (Asked by the shell rule \
             \"git push *\": Pushes need review.)? [y/N] "
        );
    }
}
