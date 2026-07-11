//! Parallel reviewer fan-out (multi-agent contract v0.2, ADR 0012).
//!
//! Phase-split execution: all provenance appends stay on the calling
//! session thread; worker threads only invoke the provider and drain its
//! stream. Event order is a pure function of the batch order, never of
//! provider completion timing, so fixture-driven logs replay
//! deterministically.

use super::companion::{companion_failure, companion_success, usage_payload, ParentedAppender};
use super::{
    canvas_snapshot_payload, context_budget_exhausted, model_input_item, AgentResultSummary,
    ModelRoundData, ModelTarget, RoundLoop, RoundLoopConfig, RoundLoopIo, RoundOutcome, Session,
    SessionError, SYSTEM_INSTRUCTIONS,
};
use crate::canvas::assemble_canvas;
use crate::permissions::PermissionDecider;
use euler_agents::{AgentResult, AgentTask, SpawnedAgent};
use euler_event::{object, EventKind, JsonObject};
use euler_provider::{
    ModelInputItem, ModelRequest, ModelRole, ModelStreamEvent, ProviderError, ProviderSet,
    ProviderStream, ReasoningChunk, StopReason,
};
use serde_json::json;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// One task prepared on the session thread: everything a worker needs, plus
/// the ids phase three records against.
struct PreparedReviewer {
    task: AgentTask,
    target: ModelTarget,
    spawned: SpawnedAgent,
    model_call_id: String,
    request: ModelRequest,
}

/// What a worker hands back: the drained round or the terminal error, plus
/// any error event it buffered instead of appending (determinism: workers
/// never append).
struct WorkerOutcome {
    round: Result<ModelRoundData, SessionError>,
    buffered_error: Option<(JsonObject, String)>,
}

impl<D: PermissionDecider> Session<D> {
    /// Run a batch of single-round, tool-free, empty-capability reviewer
    /// briefs concurrently. Outcomes return in task order; every event is
    /// appended on this thread in task order (ADR 0012).
    pub fn spawn_reviewers_parallel(
        &mut self,
        tasks: Vec<AgentTask>,
        cancel_flag: &AtomicBool,
    ) -> Result<Vec<AgentResultSummary>, SessionError> {
        if tasks.is_empty() {
            return Ok(Vec::new());
        }
        // Reject the whole batch before any event is emitted: shape, target,
        // and capability validation must not depend on batch position.
        for task in &tasks {
            validate_reviewer_brief(task)?;
            self.resolve_companion_target(task)?;
        }
        let writer = self
            .provenance
            .as_ref()
            .cloned()
            .ok_or(SessionError::CompanionProvenanceUnavailable)?;
        self.persist_new_events()?;

        // Phase 1 (session thread, batch order): shared canvas, then per
        // task agent.spawn + canvas.snapshot + model.call and the request.
        let canvas = assemble_canvas(self.bus.events(), &self.config.auto_compaction);
        if let Some(error) = context_budget_exhausted(self.config.auto_compaction, &canvas) {
            ParentedAppender {
                writer: &writer,
                bus: &mut self.bus,
                persisted_events: &mut self.persisted_events,
                session_id: &self.config.session_id.clone(),
                agent_id: &self.config.agent_id.clone(),
            }
            .append(
                EventKind::ERROR,
                object([
                    ("source", "companion".into()),
                    ("message", error.to_string().into()),
                ]),
                None,
            )?;
            return Err(error);
        }
        let canvas_input: Vec<ModelInputItem> = canvas.iter().map(model_input_item).collect();
        let mut prepared = Vec::with_capacity(tasks.len());
        for task in tasks {
            prepared.push(self.prepare_reviewer(task, &writer, &canvas, &canvas_input)?);
        }

        // Phase 2 (worker threads): concurrent provider calls. Workers
        // append nothing; they buffer round data or the terminal error.
        let outcomes = run_workers(
            &self.providers,
            &self.config.session_id,
            RoundLoopConfig {
                max_rounds: Some(1),
                transport_retries: self.config.provider_transport_retries,
                transport_retry_backoff_ms: self.config.provider_transport_retry_backoff_ms.clone(),
            },
            &prepared,
            cancel_flag,
        );

        // Phase 3 (session thread, batch order): record each reviewer's
        // round events and terminal agent.result.
        let mut summaries = Vec::with_capacity(prepared.len());
        for (reviewer, outcome) in prepared.into_iter().zip(outcomes) {
            let summary = self.record_reviewer_outcome(&writer, reviewer, outcome)?;
            summaries.push(summary);
        }
        Ok(summaries)
    }

    /// Phase 1 for one task: record `agent.spawn`, `canvas.snapshot`, and
    /// `model.call`, and build the provider request the worker will send.
    fn prepare_reviewer(
        &mut self,
        task: AgentTask,
        writer: &Arc<crate::provenance::ProvenanceWriter>,
        canvas: &[crate::CanvasItem],
        canvas_input: &[ModelInputItem],
    ) -> Result<PreparedReviewer, SessionError> {
        let target = self.resolve_companion_target(&task)?;
        let spawned = self.record_companion_spawn(&task, &target, writer)?;
        let session_id = self.config.session_id.clone();
        let child_agent_id = spawned.child_agent_id().to_owned();
        let mut appender = ParentedAppender {
            writer,
            bus: &mut self.bus,
            persisted_events: &mut self.persisted_events,
            session_id: &session_id,
            agent_id: &child_agent_id,
        };
        appender.append(
            EventKind::CANVAS_SNAPSHOT,
            canvas_snapshot_payload(canvas, self.config.auto_compaction, None, None),
            None,
        )?;
        // The task budget's max_tokens bounds the provider call itself,
        // mirroring the sequential companion loop.
        let max_output_tokens = match (self.config.max_output_tokens, task.budget().max_tokens()) {
            (Some(session_cap), Some(task_cap)) => Some(session_cap.min(task_cap)),
            (session_cap, task_cap) => session_cap.or(task_cap),
        };
        let mut model_call = object([
            ("provider", target.provider.clone().into()),
            ("model", target.model.clone().into()),
            ("canvas_items", canvas.len().into()),
            (
                "requested_reasoning_effort",
                self.config.reasoning_effort.as_str().into(),
            ),
        ]);
        if let Some(reasoning_effort) = self
            .providers
            .reasoning_effort(&target.provider, &target.model)
        {
            model_call.insert("reasoning_effort".to_owned(), reasoning_effort.into());
        }
        if let Some(max_output_tokens) = max_output_tokens {
            model_call.insert("max_output_tokens".to_owned(), max_output_tokens.into());
        }
        let model_call_id = appender.append(EventKind::MODEL_CALL, model_call, None)?.id;
        let mut input = canvas_input.to_vec();
        input.push(ModelInputItem::Message {
            role: ModelRole::User,
            content: task.task().to_owned(),
        });
        let request = ModelRequest {
            model: target.model.clone(),
            instructions: task
                .system_prompt()
                .unwrap_or(SYSTEM_INSTRUCTIONS)
                .to_owned(),
            input,
            // Reviewer briefs are tool-free by contract.
            tools: Vec::new(),
            reasoning_effort: self.config.reasoning_effort,
            max_output_tokens,
        }
        .for_target(&target.provider, &target.model);
        Ok(PreparedReviewer {
            task,
            target,
            spawned,
            model_call_id,
            request,
        })
    }

    fn record_reviewer_outcome(
        &mut self,
        writer: &Arc<crate::provenance::ProvenanceWriter>,
        reviewer: PreparedReviewer,
        outcome: WorkerOutcome,
    ) -> Result<AgentResultSummary, SessionError> {
        let PreparedReviewer {
            task,
            target,
            mut spawned,
            model_call_id,
            request: _,
        } = reviewer;
        let session_id = self.config.session_id.clone();
        let child_agent_id = spawned.child_agent_id().to_owned();
        let result = {
            let mut appender = ParentedAppender {
                writer,
                bus: &mut self.bus,
                persisted_events: &mut self.persisted_events,
                session_id: &session_id,
                agent_id: &child_agent_id,
            };
            if let Some((mut payload, parent)) = outcome.buffered_error {
                // Workers buffer the raw provider error (they carry no
                // redactor); this session-thread append is the emission
                // site, so redact here — provider HTTP error bodies can
                // echo request fragments (secrets contract).
                self.redactor
                    .redact_payload_fields(&mut payload, &["message"]);
                appender.append(EventKind::ERROR, payload, Some(parent))?;
            }
            match outcome.round {
                // The worker's terminal error carries the raw provider
                // message (HTTP error bodies can echo request fragments —
                // secrets contract). This failure string becomes the
                // agent.result error field and AgentOutcome.error, and from
                // there the code-swarm tool output and consolidated
                // artifact; redacting at this conversion point makes every
                // downstream sink inherit it. Reviewer findings (success
                // output) are model cognition and stay faithful.
                Err(error) => companion_failure(self.redactor.redact(&error.to_string())),
                Ok(data) => {
                    record_reviewer_round(&mut appender, &target, &model_call_id, &data, &task)?
                }
            }
        };
        let result_event_id = self.record_agent_result(&mut spawned, result.clone())?;
        Ok(AgentResultSummary {
            child_agent_id,
            spawn_event_id: spawned.spawn_event_id().to_owned(),
            result_event_id,
            provider: target.provider,
            model: target.model,
            result,
        })
    }
}

/// Emit one drained round's events and evaluate the same honesty rules the
/// sequential companion loop applies to a tool-free single round.
fn record_reviewer_round(
    appender: &mut ParentedAppender<'_>,
    target: &ModelTarget,
    model_call_id: &str,
    data: &ModelRoundData,
    task: &AgentTask,
) -> Result<AgentResult, SessionError> {
    let stop_reason = data
        .stop_reason
        .as_ref()
        .expect("validated finished stream");
    for reasoning in &data.reasoning {
        appender.append(
            EventKind::MODEL_REASONING,
            reasoning_payload(reasoning, target),
            Some(model_call_id.to_owned()),
        )?;
    }
    let calls = data
        .tool_calls
        .iter()
        .map(|call| json!({"id": call.id, "name": call.name, "input": call.input}))
        .collect::<Vec<_>>();
    appender.append(
        EventKind::MODEL_RESULT,
        object([
            ("provider", target.provider.clone().into()),
            ("model", target.model.clone().into()),
            ("content", data.content.clone().into()),
            ("tool_calls", calls.into()),
            ("stop_reason", stop_reason.as_str().into()),
            ("usage", usage_payload(data.usage.as_ref())),
        ]),
        Some(model_call_id.to_owned()),
    )?;

    // Budget counts OUTPUT tokens only (#58): reviewers ingest the whole
    // parent canvas as input, so counting input would exhaust every real
    // review's budget on round one — the sequential companion loop uses the
    // same output-only accounting.
    let tokens = data
        .usage
        .as_ref()
        .map(|usage| usage.output_tokens)
        .unwrap_or(0);
    if task.budget().max_tokens().is_some_and(|max| tokens > max) {
        return Ok(companion_failure("budget exhausted: max_tokens"));
    }
    if !data.tool_calls.is_empty() {
        // Reviewer briefs advertise no tools and budget zero tool calls;
        // a model that calls one anyway exhausts the budget immediately,
        // exactly as the sequential companion loop reports it.
        return Ok(companion_failure("budget exhausted: max_tool_calls"));
    }
    match stop_reason {
        StopReason::Completed => {}
        StopReason::MaxTokens | StopReason::Refusal | StopReason::Error => {
            return Ok(companion_failure(format!(
                "model round stopped without completing: {}",
                stop_reason.as_str()
            )));
        }
        StopReason::ToolUse => {
            return Ok(companion_failure(
                "model round reported tool use without tool calls",
            ));
        }
    }
    appender.append(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", data.content.clone().into())]),
        None,
    )?;
    Ok(companion_success(data.content.clone()))
}

fn reasoning_payload(reasoning: &ReasoningChunk, target: &ModelTarget) -> JsonObject {
    let mut payload = object([
        ("provider", target.provider.clone().into()),
        ("model", target.model.clone().into()),
        ("fidelity", reasoning.fidelity.as_str().into()),
        ("content", reasoning.content.clone().into()),
    ]);
    if let Some(artifact) = &reasoning.artifact {
        payload.insert("artifact".to_owned(), artifact.clone().into());
    }
    payload
}

/// Reviewer briefs are the only shape v0.2 runs in parallel: one round, no
/// tool calls, no capabilities. Anything else needs the session thread's
/// permission and tool machinery mid-flight.
fn validate_reviewer_brief(task: &AgentTask) -> Result<(), SessionError> {
    if task.budget().max_turns() != Some(1) {
        return Err(SessionError::InvalidCompanionTask(
            "parallel reviewer tasks must set max_turns = 1".to_owned(),
        ));
    }
    if task.budget().max_tool_calls() != Some(0) {
        return Err(SessionError::InvalidCompanionTask(
            "parallel reviewer tasks must set max_tool_calls = 0".to_owned(),
        ));
    }
    if !task.capabilities().is_empty() {
        return Err(SessionError::InvalidCompanionTask(
            "parallel reviewer tasks must not carry capabilities".to_owned(),
        ));
    }
    if task.budget().max_tokens() == Some(0) {
        // A zero output budget can never produce a round: fail honestly
        // before any provider call, mirroring the sequential precheck.
        return Err(SessionError::InvalidCompanionTask(
            "parallel reviewer tasks must budget at least one output token".to_owned(),
        ));
    }
    Ok(())
}

fn run_workers(
    providers: &ProviderSet,
    session_id: &str,
    config: RoundLoopConfig,
    prepared: &[PreparedReviewer],
    cancel_flag: &AtomicBool,
) -> Vec<WorkerOutcome> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = prepared
            .iter()
            .map(|reviewer| {
                let worker_config = RoundLoopConfig {
                    max_rounds: config.max_rounds,
                    transport_retries: config.transport_retries,
                    transport_retry_backoff_ms: config.transport_retry_backoff_ms.clone(),
                };
                scope.spawn(move || {
                    let mut io = WorkerIo {
                        session_id,
                        providers,
                        target: reviewer.target.clone(),
                        prepared: Some((reviewer.model_call_id.clone(), reviewer.request.clone())),
                        round: None,
                        buffered_error: None,
                    };
                    let run = RoundLoop::new(&mut io, worker_config).run(cancel_flag);
                    WorkerOutcome {
                        round: run.and_then(|()| {
                            io.round.ok_or_else(|| {
                                SessionError::InvalidCompanionTask(
                                    "reviewer round produced no data".to_owned(),
                                )
                            })
                        }),
                        buffered_error: io.buffered_error,
                    }
                })
            })
            .collect();
        // Join in batch order regardless of completion order: determinism.
        handles
            .into_iter()
            .map(|handle| {
                handle.join().unwrap_or_else(|_| WorkerOutcome {
                    // Sanitized, payload-free panic degradation (multi-agent
                    // contract): the panic payload is never persisted.
                    round: Err(SessionError::InvalidCompanionTask(
                        "reviewer worker panicked".to_owned(),
                    )),
                    buffered_error: None,
                })
            })
            .collect()
    })
}

/// Worker-side [`RoundLoopIo`]: reuses the shared loop (transport retries,
/// stream drain, truncation detection) but appends nothing — provider
/// errors are buffered for the session thread to record in batch order.
struct WorkerIo<'a> {
    session_id: &'a str,
    providers: &'a ProviderSet,
    target: ModelTarget,
    prepared: Option<(String, ModelRequest)>,
    round: Option<ModelRoundData>,
    buffered_error: Option<(JsonObject, String)>,
}

impl RoundLoopIo for WorkerIo<'_> {
    type Complete = ();

    fn session_id(&self) -> &str {
        self.session_id
    }

    fn target(&self) -> ModelTarget {
        self.target.clone()
    }

    fn prepare_model_request(
        &mut self,
        _target: &ModelTarget,
    ) -> Result<(String, ModelRequest), SessionError> {
        self.prepared.take().ok_or_else(|| {
            SessionError::InvalidCompanionTask(
                "reviewer worker prepared more than one round".to_owned(),
            )
        })
    }

    fn invoke_model(
        &mut self,
        target: &ModelTarget,
        request: ModelRequest,
    ) -> Result<ProviderStream, ProviderError> {
        self.providers.invoke(&target.provider, request)
    }

    fn emit_provider_error(
        &mut self,
        error: &ProviderError,
        model_call_id: String,
    ) -> Result<String, SessionError> {
        let mut payload = object([
            ("source", "provider".into()),
            ("message", error.to_string().into()),
        ]);
        payload.insert("category".to_owned(), error.category().as_str().into());
        self.buffered_error = Some((payload, model_call_id));
        Ok(String::new())
    }

    fn after_stream_event(
        &mut self,
        _event: &ModelStreamEvent,
        _model_call_id: &str,
    ) -> Result<(), SessionError> {
        Ok(())
    }

    fn flush_events(&mut self) {}

    fn finish_round(
        &mut self,
        _target: ModelTarget,
        _model_call_id: String,
        data: ModelRoundData,
        _cancel_flag: &AtomicBool,
    ) -> Result<RoundOutcome<()>, SessionError> {
        self.round = Some(data);
        Ok(RoundOutcome::Complete(()))
    }

    fn round_completed(&mut self) {}

    fn round_limit(&mut self) -> Result<(), SessionError> {
        // Unreachable with max_rounds = 1 and finish_round completing, but
        // the loop contract requires an answer; report it as data-less.
        Ok(())
    }
}

#[cfg(test)]
#[path = "parallel_spawn_test.rs"]
mod tests;
