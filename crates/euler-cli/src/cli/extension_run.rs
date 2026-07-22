use super::permission::CliDecider;
use anyhow::{anyhow, Result};
use euler_core::Session;

use crate::extension_cli;

pub(super) fn execute_live_extension_run(
    session: &mut Session<CliDecider>,
    request: &str,
    gated: bool,
) -> serde_json::Value {
    match parse_live_extension_request(request) {
        Ok((id, command, input)) => {
            run_live_extension_command(session, &id, &command, input, gated)
        }
        Err(error) => headless_extension_error(error.to_string()),
    }
}

fn parse_live_extension_request(request: &str) -> Result<(String, String, serde_json::Value)> {
    let mut parts = request.splitn(2, char::is_whitespace);
    let reference = parts
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("extension_run requires an extension command reference"))?;
    let input = parts
        .next()
        .ok_or_else(|| anyhow!("extension_run {reference} requires JSON input"))?
        .trim_start();
    if input.is_empty() {
        return Err(anyhow!("extension_run {reference} requires JSON input"));
    }
    let (id, command) = parse_live_extension_reference(reference)?;
    let input = serde_json::from_str(input)
        .map_err(|error| anyhow!("extension_run {reference} input must be JSON: {error}"))?;
    Ok((id, command, input))
}

fn parse_live_extension_reference(reference: &str) -> Result<(String, String)> {
    let Some((id, command)) = reference.split_once('.') else {
        return Err(anyhow!("invalid extension command reference: {reference}"));
    };
    if id.is_empty() || command.is_empty() || command.contains('.') {
        return Err(anyhow!("invalid extension command reference: {reference}"));
    }
    Ok((id.to_owned(), command.to_owned()))
}

/// Attach a linked code-swarm extension, when one is present, so the session
/// can execute the `code_swarm_review` tool (tools contract). Advertisement
/// additionally requires the extension to be enabled for the session; wiring
/// alone grants nothing, and every execution revalidates the linked package.
/// With no code-swarm linked the tool is simply absent.
pub(crate) fn wire_code_swarm<D>(session: &mut Session<D>) {
    match extension_cli::live_linked_extension_arc("code-swarm") {
        Ok(Some(extension)) => session.set_code_swarm_extension(extension),
        Ok(None) => {}
        Err(error) => {
            eprintln!("code-swarm tool unavailable: {error}");
        }
    }
}

/// Refusal text for an agent-only command reached through a control line.
/// It names the way in rather than only saying no.
pub(crate) fn agent_only_control_line_error(id: &str, command: &str) -> String {
    if id == "code-swarm" {
        return "code-swarm.review is agent-only: ask the agent for a review in ordinary turn \
                text (e.g. \"code swarm this diff\") and it will call its code_swarm_review \
                tool. Reviewer models come from the persisted /code-swarm config."
            .to_owned();
    }
    format!(
        "{id}.{command} is agent-only: it is run by the agent on your behalf, not by a control \
         line. Ask for it in ordinary turn text."
    )
}

fn run_live_extension_command(
    session: &mut Session<CliDecider>,
    id: &str,
    command: &str,
    input: serde_json::Value,
    gated: bool,
) -> serde_json::Value {
    match run_live_linked_extension_command(session, id, command, input, gated) {
        Some(result) => result,
        None => headless_extension_error(format!("unknown extension id: {id}")),
    }
}

fn run_live_linked_extension_command(
    session: &mut Session<CliDecider>,
    id: &str,
    command: &str,
    input: serde_json::Value,
    gated: bool,
) -> Option<serde_json::Value> {
    let (extension, descriptor) =
        match extension_cli::resolve_live_linked_process_command(id, command) {
            Ok(Some(resolved)) => resolved,
            Ok(None) => return None,
            Err(error) => return Some(headless_extension_error(error.to_string())),
        };
    if descriptor.invocation.is_agent_only() {
        return Some(headless_extension_error(agent_only_control_line_error(
            id, command,
        )));
    }
    if !gated && !descriptor.required_capabilities.is_empty() {
        let granted = descriptor
            .required_capabilities
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "extension {id}.{command}: granting declared capabilities for this run: {granted}"
        );
    }
    session.set_extension_enabled(id.to_owned(), true);
    let result = if gated {
        session.execute_extension_command_gated(
            &extension,
            command,
            input,
            &descriptor.required_capabilities,
        )
    } else {
        session.execute_extension_command(
            &extension,
            command,
            input,
            descriptor.required_capabilities.iter().copied(),
        )
    };
    Some(match result {
        Ok(result) => serde_json::json!({
            "type": "extension_run_result",
            "extension": id,
            "command": command,
            "result": result,
        }),
        Err(error) => headless_extension_error(error.to_string()),
    })
}

fn headless_extension_error(message: String) -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "source": "extension_run",
        "message": message,
    })
}
