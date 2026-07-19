//! Live companion loop built from session services, not a nested `Session`.

use super::{
    approval_mode_str, canvas_snapshot_payload, context_budget_exhausted, elapsed_ms,
    file_change_payload, file_diff_payload, maybe_store_pre_image, model_input_item,
    permission_decision_payload, permission_request_for_tool, validate_model_target_shape,
    ModelRoundData, ModelTarget, RoundLoop, RoundLoopConfig, RoundLoopIo, RoundOutcome, Session,
    SessionError, TurnState, SYSTEM_INSTRUCTIONS,
};
use crate::canvas::{assemble_canvas, AutoCompactionPolicy};
use crate::permissions::{ApprovalMode, PermissionDecider, PermissionGate};
use euler_agents::{generated_agent_id, AgentResult, AgentTask, SpawnedAgent};
use euler_event::{object, EventEnvelope, EventKind, JsonObject};
use euler_provider::{
    ModelInputItem, ModelRequest, ModelRole, ModelStreamEvent, ProviderError, ProviderStream,
    ReasoningChunk, ReasoningEffort, StopReason, ToolCall, Usage,
};
use euler_sdk::Capability;
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

const COMPANION_FAILURE_SUMMARY: &str = "companion failed";
const COMPANION_SUCCESS_SUMMARY: &str = "companion completed";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentResultSummary {
    pub child_agent_id: String,
    pub spawn_event_id: String,
    pub result_event_id: String,
    /// Resolved child target as the spawn event recorded it (inherited
    /// targets are resolved before recording).
    pub provider: String,
    pub model: String,
    pub result: AgentResult,
}

struct CompanionLoop<'a, D> {
    session_id: String,
    agent_id: String,
    target: ModelTarget,
    task: AgentTask,
    redactor: crate::redaction::SecretRedactor,
    workspace_root: std::path::PathBuf,
    auto_compaction: AutoCompactionPolicy,
    reasoning_effort: ReasoningEffort,
    /// Per-request provider cap inherited from the parent session
    /// (`--max-output-tokens`). The task budget's cumulative output cap is
    /// tracked separately: each round requests at most the REMAINING task
    /// budget (see `round_max_output_tokens`).
    session_max_output_tokens: Option<u64>,
    transport_retries: usize,
    transport_retry_backoff_ms: Vec<u64>,
    providers: &'a euler_provider::ProviderSet,
    tools: &'a crate::tools::ToolRegistry,
    writer: Arc<crate::provenance::ProvenanceWriter>,
    bus: &'a mut crate::EventBus,
    persisted_events: &'a mut usize,
    permissions: PermissionGate<&'a mut D>,
    turn_state: TurnState,
    /// Companion-lifetime re-teach streaks: a companion is its own model
    /// context, so it neither shares nor pollutes the parent's tracker.
    reteach: crate::tools::ReteachTracker,
    tool_calls: u32,
    /// Cumulative OUTPUT tokens only (see `add_usage`), checked against
    /// `AgentBudget::max_tokens`.
    tokens: u64,
}

pub(super) struct ParentedAppender<'a> {
    pub(super) writer: &'a Arc<crate::provenance::ProvenanceWriter>,
    pub(super) bus: &'a mut crate::EventBus,
    pub(super) persisted_events: &'a mut usize,
    pub(super) session_id: &'a str,
    pub(super) agent_id: &'a str,
}

impl<D> Session<D> {
    /// Assemble a [`ParentedAppender`] over this session's bus and
    /// persistence cursor, attributing appended events to `agent_id` under
    /// this session's id. Callers appending under the session's own agent
    /// pass a clone of `config.agent_id`; the reviewer phases pass the child
    /// agent id. The returned appender exclusively borrows the session, so
    /// build the event payload before calling this.
    pub(super) fn appender_as<'a>(
        &'a mut self,
        writer: &'a Arc<crate::provenance::ProvenanceWriter>,
        agent_id: &'a str,
    ) -> ParentedAppender<'a> {
        ParentedAppender {
            writer,
            bus: &mut self.bus,
            persisted_events: &mut self.persisted_events,
            session_id: &self.config.session_id,
            agent_id,
        }
    }
}

struct ModelResultRecord<'a> {
    content: &'a str,
    tool_calls: &'a [ToolCall],
    stop_reason: &'a StopReason,
    usage: Option<&'a Usage>,
    target: &'a ModelTarget,
    parent: String,
}

impl<D: PermissionDecider> Session<D> {
    pub fn spawn_companion(&mut self, task: AgentTask) -> Result<AgentResultSummary, SessionError> {
        // External callers have no companion cancellation source today; hand
        // the loop a flag that never trips.
        self.spawn_companion_with_cancel(task, &AtomicBool::new(false))
    }

    pub(crate) fn spawn_companion_with_cancel(
        &mut self,
        task: AgentTask,
        cancel_flag: &AtomicBool,
    ) -> Result<AgentResultSummary, SessionError> {
        let target = self.resolve_companion_target(&task)?;
        let parent_capabilities = self
            .permissions
            .configured_capabilities()
            .collect::<Vec<_>>();
        euler_agents::validate_capability_subset(
            parent_capabilities,
            task.capabilities().iter().copied(),
        )?;
        let modes = companion_modes(&self.permissions, task.capabilities());
        let writer = self
            .provenance
            .as_ref()
            .cloned()
            .ok_or(SessionError::CompanionProvenanceUnavailable)?;
        self.persist_new_events()?;

        let mut spawned = self.record_companion_spawn(&task, &target, &writer)?;
        let resolved_provider = target.provider.clone();
        let resolved_model = target.model.clone();
        let result = {
            let mut loop_ = CompanionLoop::new(
                self,
                task,
                target,
                modes,
                writer,
                spawned.child_agent_id().to_owned(),
            );
            loop_.run(cancel_flag)
        };
        let result_event_id = self.record_agent_result(&mut spawned, result.clone())?;

        Ok(AgentResultSummary {
            child_agent_id: spawned.child_agent_id().to_owned(),
            spawn_event_id: spawned.spawn_event_id().to_owned(),
            result_event_id,
            provider: resolved_provider,
            model: resolved_model,
            result,
        })
    }

    pub(super) fn resolve_companion_target(
        &self,
        task: &AgentTask,
    ) -> Result<ModelTarget, SessionError> {
        let provider = inherit_if_empty(task.provider(), &self.active_target.provider);
        let model = inherit_if_empty(task.model(), &self.active_target.model);
        let target = ModelTarget::new(provider, model);
        validate_model_target_shape(&target).map_err(SessionError::InvalidCompanionTask)?;
        if !self.providers.contains(&target.provider) {
            // Named up front (not just "a target failed") and actionable:
            // this aborts the whole spawn batch (extension callers stop on
            // the first spawn Err), so a code-swarm run with a bad target
            // must not burn the remaining reviewer slots to find out (#58).
            return Err(SessionError::InvalidCompanionTask(format!(
                "provider `{}` is not configured for this session; run /login to authenticate it or pick a different target from the reviewer-model picker",
                target.provider
            )));
        }
        Ok(target)
    }

    pub(super) fn record_companion_spawn(
        &mut self,
        task: &AgentTask,
        target: &ModelTarget,
        writer: &Arc<crate::provenance::ProvenanceWriter>,
    ) -> Result<SpawnedAgent, SessionError> {
        let child_agent_id = generated_agent_id(&self.config.agent_id);
        let mut payload = euler_agents::agent_spawn_payload(task, &child_agent_id);
        payload.insert("provider".to_owned(), target.provider.clone().into());
        payload.insert("model".to_owned(), target.model.clone().into());
        let agent_id = self.config.agent_id.clone();
        let event =
            self.appender_as(writer, &agent_id)
                .append(EventKind::AGENT_SPAWN, payload, None)?;
        self.open_agent_spawns
            .insert(event.id.clone(), child_agent_id.clone());
        Ok(SpawnedAgent::new(child_agent_id, event.id))
    }
}

impl<'a, D: PermissionDecider> CompanionLoop<'a, D> {
    fn new(
        session: &'a mut Session<D>,
        task: AgentTask,
        target: ModelTarget,
        modes: Vec<(Capability, ApprovalMode)>,
        writer: Arc<crate::provenance::ProvenanceWriter>,
        agent_id: String,
    ) -> Self {
        let mut permissions = PermissionGate::new_deny_all(session.permissions.decider_mut());
        for (capability, mode) in modes {
            permissions.set_mode(capability, mode);
        }
        Self {
            session_id: session.config.session_id.clone(),
            redactor: session.redactor.clone(),
            agent_id,
            target,
            task,
            workspace_root: session.config.root.clone(),
            auto_compaction: session.config.auto_compaction,
            reasoning_effort: session.config.reasoning_effort,
            session_max_output_tokens: session.config.max_output_tokens,
            transport_retries: session.config.provider_transport_retries,
            transport_retry_backoff_ms: session.config.provider_transport_retry_backoff_ms.clone(),
            providers: &session.providers,
            tools: &session.tools,
            writer,
            bus: &mut session.bus,
            persisted_events: &mut session.persisted_events,
            permissions,
            turn_state: TurnState::default(),
            reteach: crate::tools::ReteachTracker::default(),
            tool_calls: 0,
            tokens: 0,
        }
    }

    /// Companion rounds run through the shared [`RoundLoop`] seam, so
    /// companions inherit its transport retry (ADR 0009). max_turns
    /// maps onto the loop's round limit: it counts companion model rounds,
    /// and max_turns = 1 means at most one model round total.
    fn run(&mut self, cancel_flag: &AtomicBool) -> AgentResult {
        // A zero output budget can never produce a round: fail honestly
        // before spending a provider call on it.
        if self.remaining_output_budget() == Some(0) {
            return companion_failure("budget exhausted: max_tokens");
        }
        let config = RoundLoopConfig {
            max_rounds: self.task.budget().max_turns().map(|max| max as usize),
            transport_retries: self.transport_retries,
            transport_retry_backoff_ms: self.transport_retry_backoff_ms.clone(),
        };
        let outcome = RoundLoop::new(self, config).run(cancel_flag);
        match outcome {
            Ok(result) => result,
            // The loop's terminal error carries the raw provider message
            // (HTTP error bodies can echo request fragments — secrets
            // contract). This failure string becomes the agent.result error
            // field and AgentOutcome.error, and from there the code-swarm
            // tool output and consolidated artifact; redacting at this
            // conversion point makes every downstream sink inherit it.
            // Success output is model cognition and stays faithful.
            Err(error) => companion_failure(self.redactor.redact(&error.to_string())),
        }
    }

    /// Permission denial is a failed tool result the companion's model can
    /// adapt to, exactly as in the parent session loop; it never terminates
    /// the companion. Budgets bound the loop.
    fn execute_tool_call(&mut self, call: ToolCall) -> Result<(), SessionError> {
        let tool_call_event_id = self
            .append(
                EventKind::TOOL_CALL,
                object([
                    ("id", call.id.clone().into()),
                    ("name", call.name.clone().into()),
                    ("input", call.input.clone()),
                ]),
                None,
            )?
            .id;

        if let Some(capability) = self
            .tools
            .required_capability_for_input(&call.name, &call.input)
        {
            if self.turn_state.denied(capability) {
                self.emit_permission_denied_tool_result(call, tool_call_event_id)?;
                return Ok(());
            }
            let request = permission_request_for_tool(
                capability,
                &self.tools.permission_reason(&call.name, &call.input),
                &call.name,
                &call.input,
                self.tools,
            );
            let mode = self.permissions.mode(capability);
            let needs_prompt = mode == ApprovalMode::Ask && !self.permissions.is_granted(&request);
            let prompt_id = if needs_prompt {
                Some(
                    self.append(
                        EventKind::PERMISSION_PROMPT,
                        object([
                            ("capability", capability.as_str().into()),
                            ("reason", request.reason.clone().into()),
                        ]),
                        None,
                    )?
                    .id,
                )
            } else {
                None
            };
            let decision = self.permissions.decide_detailed(&request, mode);
            let allowed = decision.allowed();
            let mode_label = approval_mode_str(mode);
            self.append(
                EventKind::PERMISSION_DECISION,
                permission_decision_payload(&decision, mode_label, mode),
                Some(prompt_id.unwrap_or_else(|| tool_call_event_id.clone())),
            )?;
            crate::diagnostics::permission_decision(
                &self.session_id,
                capability.as_str(),
                mode_label,
                allowed,
            );
            if !allowed {
                self.turn_state.record_denial(capability);
                self.emit_permission_denied_tool_result(call, tool_call_event_id)?;
                return Ok(());
            }
        }

        self.execute_authorized_tool(call, tool_call_event_id)?;
        Ok(())
    }

    fn execute_authorized_tool(
        &mut self,
        call: ToolCall,
        tool_call_event_id: String,
    ) -> Result<(), SessionError> {
        let tool_name = call.name.clone();
        let tool_started = Instant::now();
        match self
            .tools
            .execute_with_events(&call.name, &call.input, self.bus.events())
        {
            Ok(execution) => {
                // The input format was accepted: reset this tool's re-teach
                // streak (issue #94), mirroring the parent session loop.
                self.reteach
                    .record_success(self.tools.reteach_identity(&call.name, &call.input));
                if self.record_patch_if_present(&call, &tool_call_event_id, &execution)? {
                    crate::diagnostics::tool_exec_end(
                        &self.session_id,
                        &tool_name,
                        elapsed_ms(tool_started),
                        false,
                    );
                    return Ok(());
                }
                self.record_observed_file_changes(&call.id, &execution.file_changes)?;
                self.emit_tool_success(call, execution, tool_call_event_id)?;
                crate::diagnostics::tool_exec_end(
                    &self.session_id,
                    &tool_name,
                    elapsed_ms(tool_started),
                    true,
                );
            }
            Err(error) => {
                // Rung-2 re-teaching (issue #94), companion-local streaks.
                let error = self.tools.teach_on_failure(
                    &mut self.reteach,
                    &call.name,
                    &call.input,
                    error.to_string(),
                );
                self.emit_tool_failure(call.id, call.name, error, tool_call_event_id)?;
                crate::diagnostics::tool_exec_end(
                    &self.session_id,
                    &tool_name,
                    elapsed_ms(tool_started),
                    false,
                );
            }
        }
        Ok(())
    }

    fn record_patch_if_present(
        &mut self,
        call: &ToolCall,
        tool_call_event_id: &str,
        execution: &crate::tools::ToolExecution,
    ) -> Result<bool, SessionError> {
        let Some(patch) = execution.patch.as_ref() else {
            return Ok(false);
        };
        let mut payload = object([
            ("path", patch.path.clone().into()),
            ("old", patch.before.clone().into()),
            ("new", patch.after.clone().into()),
        ]);
        self.redactor
            .redact_payload_fields(&mut payload, &["old", "new"]);
        let patch_proposed_id = self
            .append(EventKind::PATCH_PROPOSED, payload.clone(), None)?
            .id;
        if let Err(error) = self.tools.apply_patch(patch) {
            self.emit_tool_failure(
                call.id.clone(),
                execution.name.clone(),
                error.to_string(),
                tool_call_event_id.to_owned(),
            )?;
            return Ok(true);
        }
        let patch_applied_id = self
            .append(EventKind::PATCH_APPLIED, payload, Some(patch_proposed_id))?
            .id;
        let pre_image_blob = maybe_store_pre_image(self.workspace_root.as_path(), patch);
        let file_change_id = self
            .append(
                EventKind::FILE_CHANGE,
                file_change_payload(&call.id, patch, pre_image_blob.as_deref()),
                Some(patch_applied_id.clone()),
            )?
            .id;
        let mut diff_payload = file_diff_payload(&call.id, &file_change_id, patch);
        self.redactor
            .redact_payload_fields(&mut diff_payload, &["diff"]);
        self.append(EventKind::FILE_DIFF, diff_payload, Some(patch_applied_id))?;
        Ok(false)
    }

    fn record_observed_file_changes(
        &mut self,
        call_id: &str,
        changes: &[crate::ObservedFileChange],
    ) -> Result<(), SessionError> {
        for change in changes {
            let file_change_id = self
                .append(
                    EventKind::FILE_CHANGE,
                    crate::file_diff::observed_file_change_payload(call_id, "run_shell", change),
                    None,
                )?
                .id;
            let mut observed_diff = crate::file_diff::observed_file_diff_payload(
                call_id,
                &file_change_id,
                "run_shell",
                change,
            );
            self.redactor
                .redact_payload_fields(&mut observed_diff, &["diff"]);
            self.append(EventKind::FILE_DIFF, observed_diff, None)?;
        }
        Ok(())
    }

    fn emit_tool_success(
        &mut self,
        call: ToolCall,
        execution: crate::tools::ToolExecution,
        tool_call_event_id: String,
    ) -> Result<(), SessionError> {
        let mut payload = object([
            ("id", call.id.into()),
            ("name", execution.name.into()),
            ("ok", true.into()),
            ("output", self.redactor.redact(&execution.output).into()),
        ]);
        if let Some(exit_code) = execution.exit_code {
            payload.insert("exit_code".to_owned(), exit_code.into());
        }
        self.append(EventKind::TOOL_RESULT, payload, Some(tool_call_event_id))?;
        Ok(())
    }

    fn emit_tool_failure(
        &mut self,
        id: String,
        name: String,
        error: String,
        tool_call_event_id: String,
    ) -> Result<(), SessionError> {
        self.append(
            EventKind::TOOL_RESULT,
            object([
                ("id", id.into()),
                ("name", name.into()),
                ("ok", false.into()),
                ("error", self.redactor.redact(&error).into()),
            ]),
            Some(tool_call_event_id),
        )?;
        Ok(())
    }

    fn emit_permission_denied_tool_result(
        &mut self,
        call: ToolCall,
        tool_call_event_id: String,
    ) -> Result<String, SessionError> {
        Ok(self
            .append(
                EventKind::TOOL_RESULT,
                object([
                    ("id", call.id.into()),
                    ("name", call.name.into()),
                    ("ok", false.into()),
                    ("error", "permission denied".into()),
                ]),
                Some(tool_call_event_id),
            )?
            .id)
    }

    fn emit_model_result(&mut self, record: ModelResultRecord<'_>) -> Result<String, SessionError> {
        let calls = record
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "name": call.name,
                    "input": call.input,
                })
            })
            .collect::<Vec<_>>();
        Ok(self
            .append(
                EventKind::MODEL_RESULT,
                object([
                    ("provider", record.target.provider.clone().into()),
                    ("model", record.target.model.clone().into()),
                    ("content", record.content.to_owned().into()),
                    ("tool_calls", calls.into()),
                    ("stop_reason", record.stop_reason.as_str().into()),
                    ("usage", usage_payload(record.usage)),
                ]),
                Some(record.parent),
            )?
            .id)
    }

    fn emit_model_reasoning(
        &mut self,
        reasoning: &ReasoningChunk,
        target: &ModelTarget,
        parent: String,
    ) -> Result<String, SessionError> {
        let mut payload = object([
            ("provider", target.provider.clone().into()),
            ("model", target.model.clone().into()),
            ("fidelity", reasoning.fidelity.as_str().into()),
            ("content", reasoning.content.clone().into()),
        ]);
        if let Some(artifact) = &reasoning.artifact {
            payload.insert("artifact".to_owned(), artifact.clone().into());
        }
        Ok(self
            .append(EventKind::MODEL_REASONING, payload, Some(parent))?
            .id)
    }

    fn append(
        &mut self,
        kind: &'static str,
        payload: JsonObject,
        parent: Option<String>,
    ) -> Result<EventEnvelope, SessionError> {
        ParentedAppender {
            writer: &self.writer,
            bus: self.bus,
            persisted_events: self.persisted_events,
            session_id: &self.session_id,
            agent_id: &self.agent_id,
        }
        .append(kind, payload, parent)
    }

    /// Checked both before issuing a tool call and after its result is
    /// recorded: an in-flight tool always completes and records before the
    /// exhaustion terminates the loop.
    fn tool_budget_exhausted(&self) -> bool {
        self.task
            .budget()
            .max_tool_calls()
            .is_some_and(|max| self.tool_calls >= max)
    }

    /// The budget is exhausted when cumulative output EXCEEDS the cap
    /// (strictly greater): a round that lands exactly on the cap succeeds,
    /// but the next round would have a zero remaining budget and fails
    /// before it is ever requested (see `finish_round` / `run`).
    fn token_budget_exhausted(&self) -> bool {
        self.task
            .budget()
            .max_tokens()
            .is_some_and(|max| self.tokens > max)
    }

    /// Output tokens the task budget still allows: `max_tokens` minus the
    /// cumulative output so far. `None` means unbudgeted.
    fn remaining_output_budget(&self) -> Option<u64> {
        self.task
            .budget()
            .max_tokens()
            .map(|max| max.saturating_sub(self.tokens))
    }

    /// Provider cap for the NEXT round. The task budget's max_tokens must
    /// bound the provider call itself, not only the post-round accounting: a
    /// companion whose brief allows 8192 tokens must not be silently capped
    /// at the provider default because the parent session never set
    /// --max-output-tokens. It is the REMAINING budget that bounds the call,
    /// not the full cap — otherwise a multi-round companion could emit up to
    /// the full cap every round before the accounting noticed (#58).
    fn round_max_output_tokens(&self) -> Option<u64> {
        match (
            self.session_max_output_tokens,
            self.remaining_output_budget(),
        ) {
            (Some(session_cap), Some(remaining)) => Some(session_cap.min(remaining)),
            (session_cap, remaining) => session_cap.or(remaining),
        }
    }

    /// `AgentBudget::max_tokens` bounds OUTPUT (completion) tokens, not
    /// total usage: reviewers/companions see the whole session canvas as
    /// input, which routinely exceeds any output-scale budget on its own
    /// (#58). Only `usage.output_tokens` counts against it here; it is the
    /// same quantity `round_max_output_tokens` already asks the provider to
    /// cap, so the request-side cap and the round-accounting check agree.
    fn add_usage(&mut self, usage: Option<&Usage>) {
        if let Some(usage) = usage {
            self.tokens = self.tokens.saturating_add(usage.output_tokens);
        }
    }
}

/// Companion side of the shared [`RoundLoop`] seam. Unlike the session
/// implementor, companions have no live event sink: `after_stream_event`
/// and `flush_events` are no-ops because every companion event reaches the
/// bus and provenance the moment it is appended, and the round is recorded
/// wholesale in `finish_round`. `Complete` carries the companion result
/// summary ([`AgentResult`]) so budget failures, honest truncation
/// failures, and completions all exit the loop as a structured result.
impl<D: PermissionDecider> RoundLoopIo for CompanionLoop<'_, D> {
    type Complete = AgentResult;

    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn target(&self) -> ModelTarget {
        self.target.clone()
    }

    fn prepare_model_request(
        &mut self,
        target: &ModelTarget,
    ) -> Result<(String, ModelRequest), SessionError> {
        // A task that declares no parent-canvas inheritance gets none here
        // either: the flag is a privacy boundary, and honouring it only on the
        // batch path would make it a lie on this one.
        let canvas = if self.task.includes_parent_canvas() {
            assemble_canvas(self.bus.events(), &self.auto_compaction)
        } else {
            Vec::new()
        };
        if self.task.includes_parent_canvas() {
            if let Some(error) = context_budget_exhausted(self.auto_compaction, &canvas) {
                self.append(
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
        self.append(
            EventKind::CANVAS_SNAPSHOT,
            canvas_snapshot_payload(&canvas, self.auto_compaction, None, None),
            None,
        )?;
        let mut model_call = object([
            ("provider", target.provider.clone().into()),
            ("model", target.model.clone().into()),
            ("canvas_items", canvas.len().into()),
            (
                "requested_reasoning_effort",
                self.reasoning_effort.as_str().into(),
            ),
        ]);
        if let Some(reasoning_effort) = self
            .providers
            .reasoning_effort(&target.provider, &target.model)
        {
            model_call.insert("reasoning_effort".to_owned(), reasoning_effort.into());
        }
        let round_max_output_tokens = self.round_max_output_tokens();
        if let Some(max_output_tokens) = round_max_output_tokens {
            model_call.insert("max_output_tokens".to_owned(), max_output_tokens.into());
        }
        let model_call_id = self.append(EventKind::MODEL_CALL, model_call, None)?.id;
        let mut input = canvas.iter().map(model_input_item).collect::<Vec<_>>();
        if let Some(context) = self.task.explicit_context() {
            input.push(ModelInputItem::Message {
                role: ModelRole::User,
                content: context.to_owned(),
            });
        }
        input.push(ModelInputItem::Message {
            role: ModelRole::User,
            content: self.task.task().to_owned(),
        });
        let request = ModelRequest {
            model: target.model.clone(),
            instructions: self
                .task
                .system_prompt()
                .unwrap_or(SYSTEM_INSTRUCTIONS)
                .to_owned(),
            input,
            // Advertising tools a zero-tool budget forbids invites the model
            // to spend its only round on a call that instantly exhausts the
            // budget.
            tools: if self.task.budget().max_tool_calls() == Some(0) {
                Vec::new()
            } else {
                self.tools.model_tools()
            },
            reasoning_effort: self.reasoning_effort,
            max_output_tokens: round_max_output_tokens,
        }
        .for_target(&target.provider, &target.model);
        Ok((model_call_id, request))
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
        // Same chokepoint as the parent session: provider error text can
        // echo request fragments (secrets contract, "error messages").
        let mut payload = object([
            ("source", "provider".into()),
            ("message", self.redactor.redact(&error.to_string()).into()),
        ]);
        payload.insert("category".to_owned(), error.category().as_str().into());
        Ok(self
            .append(EventKind::ERROR, payload, Some(model_call_id))?
            .id)
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
        target: ModelTarget,
        model_call_id: String,
        data: ModelRoundData,
        _cancel_flag: &AtomicBool,
    ) -> Result<RoundOutcome<AgentResult>, SessionError> {
        let stop_reason = data
            .stop_reason
            .as_ref()
            .expect("validated finished stream");
        for item in &data.reasoning {
            self.emit_model_reasoning(item, &target, model_call_id.clone())?;
        }
        self.emit_model_result(ModelResultRecord {
            content: &data.content,
            tool_calls: &data.tool_calls,
            stop_reason,
            usage: data.usage.as_ref(),
            target: &target,
            parent: model_call_id,
        })?;
        self.add_usage(data.usage.as_ref());
        if self.token_budget_exhausted() {
            return Ok(RoundOutcome::Complete(companion_failure(
                "budget exhausted: max_tokens",
            )));
        }
        if data.tool_calls.is_empty() {
            // A round that stopped for any reason other than natural
            // completion has not produced the task's answer; reporting it as
            // success would launder truncation or refusal into ok=true when
            // reasoning consumed the whole output budget and the empty result
            // was summarized as "companion completed".
            match stop_reason {
                StopReason::Completed => {}
                StopReason::MaxTokens | StopReason::Refusal | StopReason::Error => {
                    return Ok(RoundOutcome::Complete(companion_failure(format!(
                        "model round stopped without completing: {}",
                        stop_reason.as_str()
                    ))));
                }
                StopReason::ToolUse => {
                    return Ok(RoundOutcome::Complete(companion_failure(
                        "model round reported tool use without tool calls",
                    )));
                }
            }
            self.append(
                EventKind::ASSISTANT_MESSAGE,
                object([("content", data.content.clone().into())]),
                None,
            )?;
            return Ok(RoundOutcome::Complete(companion_success(data.content)));
        }
        // The round wants to continue (tool calls), but a zero remaining
        // output budget means the next model round could never run. Fail
        // before executing tool calls whose results no round will observe.
        if self.remaining_output_budget() == Some(0) {
            return Ok(RoundOutcome::Complete(companion_failure(
                "budget exhausted: max_tokens",
            )));
        }
        for call in data.tool_calls {
            if self.tool_budget_exhausted() {
                return Ok(RoundOutcome::Complete(companion_failure(
                    "budget exhausted: max_tool_calls",
                )));
            }
            self.execute_tool_call(call)?;
            self.tool_calls = self.tool_calls.saturating_add(1);
            if self.tool_budget_exhausted() {
                return Ok(RoundOutcome::Complete(companion_failure(
                    "budget exhausted: max_tool_calls",
                )));
            }
        }
        Ok(RoundOutcome::Continue)
    }

    fn round_completed(&mut self) {}

    fn round_limit(&mut self) -> Result<AgentResult, SessionError> {
        Ok(companion_failure("budget exhausted: max_turns"))
    }
}

impl ParentedAppender<'_> {
    pub(super) fn append(
        &mut self,
        kind: &'static str,
        payload: JsonObject,
        parent: Option<String>,
    ) -> Result<EventEnvelope, SessionError> {
        let event = EventEnvelope::new(
            self.session_id.to_owned(),
            self.agent_id.to_owned(),
            parent,
            kind,
            payload,
        );
        let mut events = self.writer.append_parented(|_| vec![event])?;
        let event = events.pop().expect("companion events are persisted");
        self.bus.push(event.clone());
        *self.persisted_events = self.bus.events().len();
        Ok(event)
    }
}

fn companion_modes<D>(
    parent: &PermissionGate<D>,
    envelope: &[Capability],
) -> Vec<(Capability, ApprovalMode)> {
    envelope
        .iter()
        .copied()
        .map(|capability| (capability, parent.mode(capability)))
        .collect()
}

fn inherit_if_empty(value: &str, inherited: &str) -> String {
    if value.trim().is_empty() {
        inherited.to_owned()
    } else {
        value.to_owned()
    }
}

pub(super) fn companion_success(content: String) -> AgentResult {
    if content.len() > euler_agents::MAX_OUTPUT_BYTES {
        return companion_failure("companion output exceeds 64KiB");
    }
    let output = (!content.is_empty()).then_some(content);
    AgentResult::success(COMPANION_SUCCESS_SUMMARY, output.as_deref())
        .expect("bounded companion success result should be valid")
}

pub(super) fn companion_failure(error: impl AsRef<str>) -> AgentResult {
    AgentResult::failure(
        COMPANION_FAILURE_SUMMARY,
        error.as_ref(),
        Option::<&str>::None,
    )
    .expect("companion failure text should be bounded")
}

pub(super) fn usage_payload(usage: Option<&Usage>) -> Value {
    match usage {
        Some(usage) => {
            let mut value = object([
                ("input_tokens", usage.input_tokens.into()),
                ("output_tokens", usage.output_tokens.into()),
            ]);
            if let Some(cached_tokens) = usage.cached_tokens {
                value.insert("cached_tokens".to_owned(), cached_tokens.into());
            }
            if let Some(cache_write_tokens) = usage.cache_write_tokens {
                value.insert("cache_write_tokens".to_owned(), cache_write_tokens.into());
            }
            if let Some(reasoning_tokens) = usage.reasoning_tokens {
                value.insert("reasoning_tokens".to_owned(), reasoning_tokens.into());
            }
            Value::Object(value)
        }
        None => Value::Null,
    }
}

#[cfg(test)]
#[path = "companion_test.rs"]
mod tests;
