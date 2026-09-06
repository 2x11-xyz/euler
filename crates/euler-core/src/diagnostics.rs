//! Narrow diagnostics emission seam.
//!
//! Defense in depth: these helpers are the only production diagnostics path in
//! core, and their arguments are restricted to identifiers, counts, durations,
//! and status scalars. They are not a proof against future misuse, but they keep
//! user/model/tool payloads and resolved secrets out of the diagnostics log.

use euler_provider::{ProviderAttemptEvent, ProviderErrorCategory, Usage};

const TARGET: &str = "euler_core::diagnostics";

pub(crate) fn turn_start(session_id: &str) {
    tracing::info!(target: TARGET, event = "turn_start", session_id);
}

pub(crate) fn turn_end(session_id: &str, rounds: u64) {
    tracing::info!(target: TARGET, event = "turn_end", session_id, rounds);
}

pub(crate) struct ModelCallEnd<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) provider: &'a str,
    pub(crate) model: &'a str,
    pub(crate) duration_ms: u64,
    pub(crate) usage: Option<&'a Usage>,
    pub(crate) observed_output_bytes: u64,
    pub(crate) ok: bool,
}

pub(crate) fn model_call_end(record: ModelCallEnd<'_>) {
    if let Some(usage) = record.usage {
        tracing::info!(
            target: TARGET,
            event = "model_call_end",
            session_id = record.session_id,
            provider = record.provider,
            model = record.model,
            duration_ms = record.duration_ms,
            usage_available = true,
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            observed_output_bytes = record.observed_output_bytes,
            ok = record.ok
        );
    } else {
        tracing::info!(
            target: TARGET,
            event = "model_call_end",
            session_id = record.session_id,
            provider = record.provider,
            model = record.model,
            duration_ms = record.duration_ms,
            usage_available = false,
            observed_output_bytes = record.observed_output_bytes,
            ok = record.ok
        );
    }
}

pub(crate) fn provider_retry(
    session_id: &str,
    category: ProviderErrorCategory,
    attempt_id: Option<&str>,
    attempt: u64,
    backoff_ms: u64,
) {
    let attempt_id = attempt_id.unwrap_or("unknown");
    match category {
        ProviderErrorCategory::Transport => tracing::info!(
            target: TARGET,
            event = "transport_retry",
            session_id,
            attempt_id,
            attempt,
            backoff_ms
        ),
        ProviderErrorCategory::RateLimit => tracing::info!(
            target: TARGET,
            event = "rate_limit_retry",
            session_id,
            attempt_id,
            attempt,
            backoff_ms
        ),
        _ => debug_assert!(
            false,
            "non-retryable provider category reached retry diagnostics"
        ),
    }
}

pub(crate) fn provider_attempt(
    session_id: &str,
    provider: &str,
    model: &str,
    event: &ProviderAttemptEvent,
) {
    let target = ProviderAttemptTarget {
        session_id,
        provider,
        model,
    };
    match event {
        ProviderAttemptEvent::Started { attempt_id } => tracing::info!(
            target: TARGET,
            event = "provider_attempt_stage",
            session_id,
            provider,
            model,
            attempt_id,
            stage = "request_started"
        ),
        ProviderAttemptEvent::ResponseHeaders {
            attempt_id,
            elapsed_ms,
        } => provider_attempt_stage(target, attempt_id, "response_headers", *elapsed_ms),
        ProviderAttemptEvent::FirstByte {
            attempt_id,
            elapsed_ms,
        } => provider_attempt_stage(target, attempt_id, "first_byte", *elapsed_ms),
        ProviderAttemptEvent::FirstSemantic {
            attempt_id,
            elapsed_ms,
        } => provider_attempt_stage(target, attempt_id, "first_semantic", *elapsed_ms),
        ProviderAttemptEvent::Ended(summary) => {
            let timeout_stage = summary
                .outcome
                .timeout_stage()
                .map_or("none", euler_provider::ProviderTimeoutStage::as_str);
            tracing::info!(
                target: TARGET,
                event = "provider_attempt_end",
                session_id,
                provider,
                model,
                attempt_id = summary.attempt_id,
                outcome = summary.outcome.as_str(),
                timeout_stage,
                elapsed_ms = summary.elapsed_ms,
                response_headers_ms = ?summary.response_headers_ms,
                first_byte_ms = ?summary.first_byte_ms,
                first_semantic_ms = ?summary.first_semantic_ms,
                last_transport_activity_ms = ?summary.last_transport_activity_ms,
                last_semantic_activity_ms = ?summary.last_semantic_activity_ms
            );
        }
    }
}

#[derive(Clone, Copy)]
struct ProviderAttemptTarget<'a> {
    session_id: &'a str,
    provider: &'a str,
    model: &'a str,
}

fn provider_attempt_stage(
    target: ProviderAttemptTarget<'_>,
    attempt_id: &str,
    stage: &str,
    elapsed_ms: u64,
) {
    tracing::info!(
        target: TARGET,
        event = "provider_attempt_stage",
        session_id = target.session_id,
        provider = target.provider,
        model = target.model,
        attempt_id,
        stage,
        elapsed_ms
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
