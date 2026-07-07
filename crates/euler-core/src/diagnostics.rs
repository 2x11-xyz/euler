//! Narrow diagnostics emission seam.
//!
//! Defense in depth: these helpers are the only production diagnostics path in
//! core, and their arguments are restricted to identifiers, counts, durations,
//! and status scalars. They are not a proof against future misuse, but they keep
//! user/model/tool payloads and resolved secrets out of the diagnostics log.

use euler_provider::Usage;

const TARGET: &str = "euler_core::diagnostics";

pub(crate) fn turn_start(session_id: &str) {
    tracing::info!(target: TARGET, event = "turn_start", session_id);
}

pub(crate) fn turn_end(session_id: &str, rounds: u64) {
    tracing::info!(target: TARGET, event = "turn_end", session_id, rounds);
}

pub(crate) fn model_call_end(
    session_id: &str,
    provider: &str,
    model: &str,
    duration_ms: u64,
    usage: Option<&Usage>,
    ok: bool,
) {
    let input_tokens = usage.map_or(0, |usage| usage.input_tokens);
    let output_tokens = usage.map_or(0, |usage| usage.output_tokens);
    tracing::info!(
        target: TARGET,
        event = "model_call_end",
        session_id,
        provider,
        model,
        duration_ms,
        input_tokens,
        output_tokens,
        ok
    );
}

pub(crate) fn transport_retry(session_id: &str, attempt: u64, backoff_ms: u64) {
    tracing::info!(
        target: TARGET,
        event = "transport_retry",
        session_id,
        attempt,
        backoff_ms
    );
}

pub(crate) fn tool_exec_end(session_id: &str, tool: &str, duration_ms: u64, ok: bool) {
    tracing::info!(
        target: TARGET,
        event = "tool_exec_end",
        session_id,
        tool,
        duration_ms,
        ok
    );
}

pub(crate) fn permission_decision(session_id: &str, capability: &str, mode: &str, allowed: bool) {
    tracing::info!(
        target: TARGET,
        event = "permission_decision",
        session_id,
        capability,
        mode,
        allowed
    );
}

pub(crate) fn extension_command_end(
    session_id: &str,
    extension_id: &str,
    command: &str,
    duration_ms: u64,
    ok: bool,
) {
    tracing::info!(
        target: TARGET,
        event = "extension_command_end",
        session_id,
        extension_id,
        command,
        duration_ms,
        ok
    );
}

pub(crate) fn round_observer_end(
    session_id: &str,
    rounds: u64,
    duration_ms: u64,
    failed_stage: Option<&'static str>,
) {
    tracing::info!(
        target: TARGET,
        event = "round_observer_end",
        session_id,
        rounds,
        duration_ms,
        ok = failed_stage.is_none(),
        failed_stage
    );
}

pub(crate) fn provenance_append_end(session_id: &str, events: u64, bytes: u64, duration_ms: u64) {
    tracing::debug!(
        target: TARGET,
        event = "provenance_append_end",
        session_id,
        events,
        bytes,
        duration_ms
    );
}
