//! Round-boundary observer: a product-neutral brief -> companion -> apply
//! chain run mid-turn at a configured cadence of driver rounds.

use super::{elapsed_ms, AgentResultSummary, Session};
use crate::permissions::PermissionDecider;
use euler_agents::{AgentBudget, AgentTask};
use euler_sdk::Extension;
use serde_json::{json, Value};
use std::num::NonZeroU64;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

const DEFAULT_OBSERVER_PERSONA: &str = "round-observer";

/// Cadence and command pair for the round-boundary observer. Core stays
/// generic over any (brief, apply) command pair on the wired extension;
/// interpretation of the brief and the apply input is the extension's.
#[derive(Clone, Debug)]
pub struct RoundObserverConfig {
    /// Run the observer chain at every mid-turn boundary where the
    /// completed driver round count is a multiple of this cadence.
    pub cadence_rounds: NonZeroU64,
    /// Extension command producing either an observer task envelope or the
    /// generic no-work envelope `{ "status": "idle" }`. A task envelope has
    /// `task` (required string), optional `provider`/`model` (both or neither),
    /// optional `system_prompt` (string), optional `budget` (`max_turns`,
    /// `max_tool_calls`, `max_tokens`), and an opaque `apply` value passed back
    /// untouched.
    pub brief_command: String,
    /// Extension command receiving `{ "apply": <brief apply value>,
    /// "companion": { ok, summary, output, error, ids... } }`.
    pub apply_command: String,
}

impl<D: PermissionDecider> Session<D> {
    /// Fail-open by contract: any brief/companion/apply failure is recorded
    /// to diagnostics and never fails the driver turn.
    pub(super) fn observe_round_boundary(&mut self, rounds: u64, cancel_flag: &AtomicBool) {
        let Some(config) = self.config.round_observer.clone() else {
            return;
        };
        // Cadence check stays isolated here so a future event-count force
        // trigger (bounded-page pressure) can join it. `rounds` >= 1: the
        // boundary fires only after a completed round.
        if !rounds.is_multiple_of(config.cadence_rounds.get()) {
            return;
        }
        let Some(extension) = self.observer_extension.clone() else {
            return;
        };
        let started = Instant::now();
        let failed_stage = self
            .run_observer_chain(&config, extension.as_ref(), cancel_flag)
            .err();
        crate::diagnostics::round_observer_end(
            &self.config.session_id,
            rounds,
            elapsed_ms(started),
            failed_stage,
        );
    }

    fn run_observer_chain(
        &mut self,
        config: &RoundObserverConfig,
        extension: &dyn Extension,
        cancel_flag: &AtomicBool,
    ) -> Result<(), &'static str> {
        let granted = extension.manifest().capabilities;
        let brief = self
            .execute_extension_command(
                extension,
                &config.brief_command,
                Value::Null,
                granted.iter().copied(),
            )
            .map_err(|_| "brief")?;
        let Some((task, apply)) = observer_task(&brief)? else {
            return Ok(());
        };
        // The observer companion is a one-turn generation task: it only
        // PRODUCES the observation. It runs with an empty capability set —
        // extension-host capabilities (artifact-write, context-slot, ...)
        // are not tool-permission capabilities, so granting the manifest set
        // here would fail companion subset validation against the parent
        // session and reject every spawn. All writes happen in the apply
        // command, which core executes with the extension's manifest grant.
        let summary = self
            .spawn_companion_with_cancel(task, cancel_flag)
            .map_err(|_| "companion")?;
        let input = json!({ "apply": apply, "companion": companion_payload(&summary) });
        self.execute_extension_command(extension, &config.apply_command, input, granted)
            .map_err(|_| "apply")?;
        Ok(())
    }
}

fn observer_task(brief: &Value) -> Result<Option<(AgentTask, Value)>, &'static str> {
    match brief.get("status") {
        Some(Value::String(status)) if status == "idle" => {
            const TASK_FIELDS: [&str; 7] = [
                "task",
                "provider",
                "model",
                "persona",
                "system_prompt",
                "budget",
                "apply",
            ];
            if TASK_FIELDS.iter().any(|field| brief.get(*field).is_some()) {
                return Err("envelope");
            }
            return Ok(None);
        }
        Some(_) => return Err("envelope"),
        None => {}
    }
    let text = brief
        .get("task")
        .and_then(Value::as_str)
        .ok_or("envelope")?;
    let provider = brief.get("provider").and_then(Value::as_str).unwrap_or("");
    let model = brief.get("model").and_then(Value::as_str).unwrap_or("");
    // The observer spawns under the persona the brief declares (falling back
    // to the generic default). The extension's self-event exclusion and
    // incomplete-span fence key off this persona — hardcoding a different
    // one silently defeats them, feeding the previous observer's own hints
    // back into the next observation window as evidence (review #105 F1).
    let persona = brief
        .get("persona")
        .and_then(Value::as_str)
        .filter(|persona| !persona.is_empty())
        .unwrap_or(DEFAULT_OBSERVER_PERSONA);
    let mut task = match (provider.is_empty(), model.is_empty()) {
        (true, true) => AgentTask::new_inheriting_target(text, persona),
        (false, false) => AgentTask::new(text, persona, provider, model),
        _ => return Err("envelope"),
    }
    .map_err(|_| "envelope")?;
    match brief.get("system_prompt") {
        None | Some(Value::Null) => {}
        Some(value) => {
            let system_prompt = value.as_str().ok_or("envelope")?;
            task = task
                .with_system_prompt(system_prompt)
                .map_err(|_| "envelope")?;
        }
    }
    if let Some(budget) = brief.get("budget") {
        let budget = AgentBudget::new(
            budget_u32(budget, "max_turns")?,
            budget_u32(budget, "max_tool_calls")?,
            budget_field(budget, "max_tokens")?,
        )
        .map_err(|_| "envelope")?;
        task = task.with_budget(budget);
    }
    Ok(Some((
        task,
        brief.get("apply").cloned().unwrap_or(Value::Null),
    )))
}

fn budget_u32(budget: &Value, key: &str) -> Result<Option<u32>, &'static str> {
    budget_field(budget, key)?
        .map(u32::try_from)
        .transpose()
        .map_err(|_| "envelope")
}

fn budget_field(budget: &Value, key: &str) -> Result<Option<u64>, &'static str> {
    match budget.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or("envelope"),
    }
}

fn companion_payload(summary: &AgentResultSummary) -> Value {
    json!({
        "ok": summary.result.ok(),
        "summary": summary.result.summary(),
        "output": summary.result.output(),
        "error": summary.result.error(),
        "child_agent_id": summary.child_agent_id,
        "spawn_event_id": summary.spawn_event_id,
        "result_event_id": summary.result_event_id,
    })
}

#[cfg(test)]
#[path = "observer_test.rs"]
mod tests;
