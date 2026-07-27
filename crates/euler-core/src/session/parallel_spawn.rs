//! Parallel reviewer fan-out (multi-agent contract v0.2, ADR 0012).
//!
//! Phase-split execution: all provenance appends stay on the calling
//! session thread; worker threads only invoke the provider and drain its
//! stream. Event order is a pure function of the batch order, never of
//! provider completion timing, so fixture-driven logs replay
//! deterministically.

use super::companion::{
    companion_failure, companion_success, model_result_payload, ModelResultRecord, ParentedAppender,
};
use super::{
    canvas_snapshot_payload, context_budget_exhausted, model_input_item, AgentResultSummary,
    ContextLimitConfig, ModelRoundData, ModelTarget, RoundLoop, RoundLoopConfig, RoundLoopIo,
    RoundOutcome, Session, SessionError, SYSTEM_INSTRUCTIONS,
};
use crate::canvas::assemble_canvas_prefolded;
use crate::permissions::PermissionDecider;
use euler_agents::{AgentResult, AgentTask, SpawnedAgent};
use euler_event::{object, EventKind, JsonObject};
use euler_provider::{
    ModelInputItem, ModelRequest, ModelRole, ModelStreamEvent, ProviderError, ProviderSet,
    ProviderStream, ReasoningChunk, StopReason,
};
use euler_sdk::CancellationToken;
use std::sync::Arc;

const REVIEWER_UNKNOWN_OUTCOME_MESSAGE: &str = "reviewer worker outcome unavailable";

/// One task prepared on the session thread: everything a worker needs, plus
/// the ids phase three records against.
struct PreparedReviewer {
    task: AgentTask,
    target: ModelTarget,
    spawned: SpawnedAgent,
    preparation: ReviewerPreparation,
}

/// Context admission either produces one honest provider call lifecycle or
/// rejects the reviewer before that lifecycle begins.
enum ReviewerPreparation {
    Ready {
        model_call_id: String,
        request: ModelRequest,
    },
    Rejected {
        message: String,
    },
}

/// What a worker hands back: the drained round or the terminal error, plus
/// any error event it buffered instead of appending (determinism: workers
/// never append).
enum WorkerOutcome {
    Rejected,
    Ran(Box<WorkerRunOutcome>),
}

struct WorkerRunOutcome {
    round: Result<ModelRoundData, SessionError>,
    buffered_error: Option<(JsonObject, String)>,
}

struct ReviewerRecordContext<'a> {
    task: &'a AgentTask,
    target: &'a ModelTarget,
    child_agent_id: &'a str,
    model_call_id: &'a str,
}

impl<D: PermissionDecider> Session<D> {
    /// Run a batch of single-round, tool-free, empty-capability reviewer
    /// briefs concurrently. Outcomes return in task order; every event is
    /// appended on this thread in task order (ADR 0012).
    pub fn spawn_reviewers_parallel(
        &mut self,
        tasks: Vec<AgentTask>,
        cancellation: &CancellationToken,
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

        // Phase 1 (session thread, batch order): assemble the parent canvas
        // only when at least one task explicitly requests it. Self-contained
        // review briefs do not inherit ambient session history. The
        // project-context fold happens once here and serves both canvas
        // assembly and the per-task snapshot below; a fold error keeps its
        // original precedence (reported after the canvas budget check) and,
        // as before, contributes no pinned item to the assembled canvas.
        let folded = crate::project_context::fold_project_context(self.bus.events());
        let include_parent_canvas = tasks.iter().any(AgentTask::includes_parent_canvas);
        let canvas = if include_parent_canvas {
            assemble_canvas_prefolded(
                self.bus.events(),
                &self.config.auto_compaction,
                &std::collections::BTreeSet::new(),
                folded.as_ref().ok().and_then(|fold| fold.admitted()),
            )
        } else {
            Vec::new()
        };
        if include_parent_canvas {
            if let Some(error) = context_budget_exhausted(self.config.auto_compaction, &canvas) {
                let agent_id = self.config.agent_id.clone();
                self.appender_as(&writer, &agent_id).append(
                    EventKind::ERROR,
                    object([
                        ("source", "companion".into()),
                        ("message", error.to_string().into()),
                    ]),
                    None,
                )?;
                return Err(error);
            }
        }
        // One immutable pre-fan-out project-context snapshot for the whole
        // batch: parallel inheriting children can never diverge because a
        // file changed during the batch (they read no files at all).
        let project_context = match folded {
            Ok(fold) => fold,
            Err(error) => {
                let error = SessionError::ProjectContextInvalid(error.to_string());
                let agent_id = self.config.agent_id.clone();
                self.appender_as(&writer, &agent_id).append(
                    EventKind::ERROR,
                    object([
                        ("source", "companion".into()),
                        ("message", error.to_string().into()),
                    ]),
                    None,
                )?;
                return Err(error);
            }
        };
        let mut prepared = Vec::with_capacity(tasks.len());
        for task in tasks {
            prepared.push(self.prepare_reviewer(task, &writer, &canvas, &project_context)?);
        }

        // Phase 2 (worker threads): concurrent provider calls. Workers
        // append nothing; they buffer round data or the terminal error.
        let outcomes = run_workers(
            &self.providers,
            &self.config.session_id,
            RoundLoopConfig {
                max_rounds: Some(1),
                provider_retries: self.config.provider_transport_retries,
                provider_retry_backoff_ms: self.config.provider_transport_retry_backoff_ms.clone(),
            },
            &prepared,
            cancellation,
        );

        // Phase 3 (session thread, batch order): record each reviewer's
        // round events and terminal agent.result.
        let mut summaries = Vec::with_capacity(prepared.len());
        for (reviewer, outcome) in prepared.into_iter().zip(outcomes) {
            let summary = self.record_reviewer_outcome(&writer, reviewer, outcome)?;
            summaries.push(summary);
        }
        if cancellation.is_cancelled() {
            return Err(SessionError::Cancelled);
        }
        Ok(summaries)
    }

    /// Phase 1 for one task: record `agent.spawn` and `canvas.snapshot`, admit
    /// the request against the context window, then record `model.call` only
    /// when a worker will actually dispatch it.
    fn prepare_reviewer(
        &mut self,
        task: AgentTask,
        writer: &Arc<crate::provenance::ProvenanceWriter>,
        canvas: &[crate::CanvasItem],
        project_context: &crate::project_context::ProjectContextFold,
    ) -> Result<PreparedReviewer, SessionError> {
        let target = self.resolve_companion_target(&task)?;
        let spawned = self.record_companion_spawn(&task, &target, writer)?;
        let child_agent_id = spawned.child_agent_id().to_owned();
        let mut task_canvas: Vec<crate::CanvasItem> = if task.includes_parent_canvas() {
            canvas.to_vec()
        } else {
            Vec::new()
        };
        // Explicit child project-context policy (ADR 0017): filter the whole
        // classified item family unless the spawn recorded `inherit`, and
        // supply the shared pre-fan-out snapshot under `inherit` even
        // without the parent canvas.
        super::apply_child_project_context_policy(
            &mut task_canvas,
            task.project_context(),
            project_context,
        );
        let snapshot_payload =
            canvas_snapshot_payload(&task_canvas, self.config.auto_compaction, None, None);
        self.appender_as(writer, &child_agent_id).append(
            EventKind::CANVAS_SNAPSHOT,
            snapshot_payload,
            None,
        )?;
        // The task budget's max_tokens bounds the provider call itself,
        // mirroring the sequential companion loop.
        let max_output_tokens = match (self.config.max_output_tokens, task.budget().max_tokens()) {
            (Some(session_cap), Some(task_cap)) => Some(session_cap.min(task_cap)),
            (session_cap, task_cap) => session_cap.or(task_cap),
        };
        let input = reviewer_input(&task_canvas, &task);
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
        // One oversized reviewer must not sink the batch: the swarm's K-of-N
        // summary exists to report exactly this kind of partial failure, so
        // record an error event for this child and let its siblings run. The
        // rejection precedes model.call: no provider lifecycle began.
        if let Some(message) = self.reviewer_context_overflow(&request) {
            self.appender_as(writer, &child_agent_id).append(
                EventKind::ERROR,
                object([
                    ("source", "companion".into()),
                    ("message", message.clone().into()),
                ]),
                None,
            )?;
            return Ok(PreparedReviewer {
                task,
                target,
                spawned,
                preparation: ReviewerPreparation::Rejected { message },
            });
        }

        let mut model_call =
            self.reviewer_model_call_payload(&target, task_canvas.len(), max_output_tokens);
        if let Some(digest) = super::canvas_project_context_digest(&task_canvas, project_context) {
            model_call.insert("project_context_digest".to_owned(), digest.into());
        }
        let model_call_id = self
            .appender_as(writer, &child_agent_id)
            .append(EventKind::MODEL_CALL, model_call, None)?
            .id;
        Ok(PreparedReviewer {
            task,
            target,
            spawned,
            preparation: ReviewerPreparation::Ready {
                model_call_id,
                request,
            },
        })
    }

    /// The `model.call` payload for one reviewer. `canvas_items` reports what
    /// this child actually receives, which is zero for canvas-disabled briefs.
    fn reviewer_model_call_payload(
        &self,
        target: &ModelTarget,
        canvas_items: usize,
        max_output_tokens: Option<u64>,
    ) -> JsonObject {
        let mut model_call = object([
            ("provider", target.provider.clone().into()),
            ("model", target.model.clone().into()),
            ("canvas_items", canvas_items.into()),
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
        model_call
    }

    /// Why this reviewer cannot be dispatched under the shared deterministic
    /// request proxy, if the model's context window is known.
    fn reviewer_context_overflow(&self, request: &ModelRequest) -> Option<String> {
        let limit = self
            .config
            .context_limit
            .as_ref()
            .map(ContextLimitConfig::limit_tokens)?;
        let output_reserve = request.max_output_tokens.unwrap_or(0);
        match crate::project_context::request_required_tokens(request, output_reserve) {
            Some(required) if crate::project_context::fits_context_limit(required, limit) => None,
            Some(required) => Some(format!(
                "reviewer request exceeds context limit: {required} tokens required > {limit}"
            )),
            None => Some(format!(
                "reviewer request exceeds context limit: token accounting overflowed for {limit} available tokens"
            )),
        }
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
            preparation,
        } = reviewer;
        let (model_call_id, round, buffered_error) = match (preparation, outcome) {
            (ReviewerPreparation::Rejected { message }, WorkerOutcome::Rejected) => {
                return self.record_rejected_reviewer(target, spawned, message);
            }
            (
                ReviewerPreparation::Ready {
                    model_call_id,
                    request: _,
                },
                WorkerOutcome::Ran(outcome),
            ) => {
                let WorkerRunOutcome {
                    round,
                    buffered_error,
                } = *outcome;
                (model_call_id, round, buffered_error)
            }
            _ => {
                return Err(SessionError::InvalidCompanionTask(
                    "reviewer preparation and worker outcome disagree".to_owned(),
                ));
            }
        };
        let child_agent_id = spawned.child_agent_id().to_owned();
        let result = match (round, buffered_error) {
            (Ok(data), None) => self.record_successful_reviewer_round(
                writer,
                ReviewerRecordContext {
                    task: &task,
                    target: &target,
                    child_agent_id: &child_agent_id,
                    model_call_id: &model_call_id,
                },
                &data,
            )?,
            (Ok(_), buffered_error) => {
                self.append_reviewer_failure_terminal(
                    writer,
                    &child_agent_id,
                    &model_call_id,
                    buffered_error,
                    None,
                )?;
                companion_failure(REVIEWER_UNKNOWN_OUTCOME_MESSAGE)
            }
            // The worker's terminal error carries the raw provider
            // message (HTTP error bodies can echo request fragments —
            // secrets contract). This failure string becomes the
            // agent.result error field and AgentOutcome.error, and from
            // there the code-swarm tool output and consolidated
            // artifact; redacting at this conversion point makes every
            // downstream sink inherit it. Reviewer findings (success
            // output) are model cognition and stay faithful.
            (Err(error), buffered_error) => {
                self.append_reviewer_failure_terminal(
                    writer,
                    &child_agent_id,
                    &model_call_id,
                    buffered_error,
                    Some(&error),
                )?;
                companion_failure(self.redactor.redact(&error.to_string()))
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

    fn record_successful_reviewer_round(
        &mut self,
        writer: &Arc<crate::provenance::ProvenanceWriter>,
        context: ReviewerRecordContext<'_>,
        data: &ModelRoundData,
    ) -> Result<AgentResult, SessionError> {
        let model_result = model_result_payload(
            &ModelResultRecord {
                content: &data.content,
                tool_calls: &data.tool_calls,
                stop_reason: data
                    .stop_reason
                    .as_ref()
                    .expect("validated finished stream"),
                usage: data.usage.as_ref(),
                target: context.target,
                parent: context.model_call_id.to_owned(),
            },
            &self.providers,
        );
        let mut appender = self.appender_as(writer, context.child_agent_id);
        record_reviewer_round(
            &mut appender,
            context.target,
            context.model_call_id,
            data,
            context.task,
            model_result,
        )
    }

    /// Close a prepared reviewer's provider lifecycle exactly once before its
    /// `agent.result` is recorded. A worker can disappear before RoundLoop
    /// has a chance to buffer an error (pre-dispatch cancellation or panic);
    /// those unknown outcomes receive a sanitized session recovery closure.
    fn append_reviewer_failure_terminal(
        &mut self,
        writer: &Arc<crate::provenance::ProvenanceWriter>,
        child_agent_id: &str,
        model_call_id: &str,
        buffered_error: Option<(JsonObject, String)>,
        error: Option<&SessionError>,
    ) -> Result<(), SessionError> {
        let mut payload = match buffered_error {
            Some((payload, worker_parent)) => {
                debug_assert_eq!(worker_parent, model_call_id);
                payload
            }
            None if matches!(error, Some(SessionError::Cancelled)) => {
                super::round_loop::model_call_cancelled_payload()
            }
            None => object([
                ("source", "session".into()),
                ("message", REVIEWER_UNKNOWN_OUTCOME_MESSAGE.into()),
                ("recovery_closure", true.into()),
            ]),
        };
        // Workers carry no redactor. Provider error bodies can echo request
        // fragments, so the session-thread emission site owns redaction.
        self.redactor
            .redact_payload_fields(&mut payload, &["message"]);
        self.appender_as(writer, child_agent_id).append(
            EventKind::ERROR,
            payload,
            Some(model_call_id.to_owned()),
        )?;
        Ok(())
    }

    fn record_rejected_reviewer(
        &mut self,
        target: ModelTarget,
        mut spawned: SpawnedAgent,
        message: String,
    ) -> Result<AgentResultSummary, SessionError> {
        let child_agent_id = spawned.child_agent_id().to_owned();
        let result = companion_failure(message);
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
    model_result: JsonObject,
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
    appender.append(
        EventKind::MODEL_RESULT,
        model_result,
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

fn reviewer_input(task_canvas: &[crate::CanvasItem], task: &AgentTask) -> Vec<ModelInputItem> {
    let mut input = task_canvas.iter().map(model_input_item).collect::<Vec<_>>();
    if let Some(context) = task.explicit_context() {
        input.push(ModelInputItem::Message {
            role: ModelRole::User,
            content: context.to_owned(),
        });
    }
    input.push(ModelInputItem::Message {
        role: ModelRole::User,
        content: task.task().to_owned(),
    });
    input
}

fn run_workers(
    providers: &ProviderSet,
    session_id: &str,
    config: RoundLoopConfig,
    prepared: &[PreparedReviewer],
    cancellation: &CancellationToken,
) -> Vec<WorkerOutcome> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = prepared
            .iter()
            .map(|reviewer| {
                let ReviewerPreparation::Ready {
                    model_call_id,
                    request,
                } = &reviewer.preparation
                else {
                    return None;
                };
                let worker_config = RoundLoopConfig {
                    max_rounds: config.max_rounds,
                    provider_retries: config.provider_retries,
                    provider_retry_backoff_ms: config.provider_retry_backoff_ms.clone(),
                };
                let worker_cancellation = cancellation.clone();
                let target = reviewer.target.clone();
                let model_call_id = model_call_id.clone();
                let request = request.clone();
                Some(scope.spawn(move || {
                    let mut io = WorkerIo {
                        session_id,
                        providers,
                        target,
                        prepared: Some((model_call_id, request)),
                        round: None,
                        buffered_error: None,
                        cancellation: worker_cancellation.clone(),
                    };
                    let run = RoundLoop::new(&mut io, worker_config).run(&worker_cancellation);
                    WorkerOutcome::Ran(Box::new(WorkerRunOutcome {
                        round: run.and_then(|()| {
                            io.round.ok_or_else(|| {
                                SessionError::InvalidCompanionTask(
                                    "reviewer round produced no data".to_owned(),
                                )
                            })
                        }),
                        buffered_error: io.buffered_error,
                    }))
                }))
            })
            .collect();
        // Join in batch order regardless of completion order: determinism.
        handles
            .into_iter()
            .map(|handle| match handle {
                None => WorkerOutcome::Rejected,
                Some(handle) => handle.join().unwrap_or_else(|_| {
                    WorkerOutcome::Ran(Box::new(WorkerRunOutcome {
                        // Sanitized, payload-free panic degradation (multi-agent
                        // contract): the panic payload is never persisted.
                        round: Err(SessionError::InvalidCompanionTask(
                            "reviewer worker panicked".to_owned(),
                        )),
                        buffered_error: None,
                    }))
                }),
            })
            .collect()
    })
}

/// Worker-side [`RoundLoopIo`]: reuses the shared loop (transient retries,
/// stream drain, truncation detection) but appends nothing — provider
/// errors are buffered for the session thread to record in batch order.
struct WorkerIo<'a> {
    session_id: &'a str,
    providers: &'a ProviderSet,
    target: ModelTarget,
    prepared: Option<(String, ModelRequest)>,
    round: Option<ModelRoundData>,
    buffered_error: Option<(JsonObject, String)>,
    cancellation: CancellationToken,
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
        self.providers.invoke_interruptibly(
            &target.provider,
            request,
            super::provider_cancellation(self.cancellation.clone()),
        )
    }

    fn emit_provider_error(
        &mut self,
        error: &ProviderError,
        model_call_id: String,
    ) -> Result<String, SessionError> {
        if error.request_outcome_unknown() {
            // The provider request thread disappeared after dispatch may have
            // begun. Leave the error unbuffered so phase three records the
            // canonical session recovery closure for an unknown outcome.
            return Ok(String::new());
        }
        let mut payload = object([
            ("source", "provider".into()),
            ("message", error.to_string().into()),
        ]);
        payload.insert("category".to_owned(), error.category().as_str().into());
        self.buffered_error = Some((payload, model_call_id));
        Ok(String::new())
    }

    fn emit_model_call_cancelled(&mut self, model_call_id: String) -> Result<String, SessionError> {
        self.buffered_error = Some((
            super::round_loop::model_call_cancelled_payload(),
            model_call_id,
        ));
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
        _cancellation: &CancellationToken,
        _another_round_available: bool,
    ) -> Result<RoundOutcome<()>, SessionError> {
        self.round = Some(data);
        Ok(RoundOutcome::Complete(()))
    }

    fn round_completed(&mut self) {}

    fn round_limit(&mut self, _cancellation: &CancellationToken) -> Result<(), SessionError> {
        // Unreachable with max_rounds = 1 and finish_round completing, but
        // the loop contract requires an answer; report it as data-less.
        Ok(())
    }
}

#[cfg(test)]
#[path = "parallel_spawn_test.rs"]
mod tests;
