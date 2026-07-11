//! Live companion loop built from session services, not a nested `Session`.

use super::{
    approval_mode_str, canvas_snapshot_payload, context_budget_exhausted, elapsed_ms,
    file_change_payload, file_diff_payload, maybe_store_pre_image, model_input_item,
    permission_decision_payload, permission_request_for_tool, used_tokens,
    validate_model_target_shape, ModelRoundData, ModelTarget, RoundLoop, RoundLoopConfig,
    RoundLoopIo, RoundOutcome, Session, SessionError, TurnState, SYSTEM_INSTRUCTIONS,
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
    workspace_root: std::path::PathBuf,
    auto_compaction: AutoCompactionPolicy,
    reasoning_effort: ReasoningEffort,
    max_output_tokens: Option<u64>,
    transport_retries: usize,
    transport_retry_backoff_ms: Vec<u64>,
    providers: &'a euler_provider::ProviderSet,
    tools: &'a crate::tools::ToolRegistry,
    writer: Arc<crate::provenance::ProvenanceWriter>,
    bus: &'a mut crate::EventBus,
    persisted_events: &'a mut usize,
    permissions: PermissionGate<&'a mut D>,
    turn_state: TurnState,
    tool_calls: u32,
    tokens: u64,
}

struct ParentedAppender<'a> {
    writer: &'a Arc<crate::provenance::ProvenanceWriter>,
    bus: &'a mut crate::EventBus,
    persisted_events: &'a mut usize,
    session_id: &'a str,
    agent_id: &'a str,
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

    fn resolve_companion_target(&self, task: &AgentTask) -> Result<ModelTarget, SessionError> {
        let provider = inherit_if_empty(task.provider(), &self.active_target.provider);
        let model = inherit_if_empty(task.model(), &self.active_target.model);
        let target = ModelTarget::new(provider, model);
        validate_model_target_shape(&target).map_err(SessionError::InvalidCompanionTask)?;
        if !self.providers.contains(&target.provider) {
            return Err(SessionError::InvalidCompanionTask(format!(
                "provider is not configured: {}",
                target.provider
            )));
        }
        Ok(target)
    }

    fn record_companion_spawn(
        &mut self,
        task: &AgentTask,
        target: &ModelTarget,
        writer: &Arc<crate::provenance::ProvenanceWriter>,
    ) -> Result<SpawnedAgent, SessionError> {
        let child_agent_id = generated_agent_id(&self.config.agent_id);
        let mut payload = euler_agents::agent_spawn_payload(task, &child_agent_id);
        payload.insert("provider".to_owned(), target.provider.clone().into());
        payload.insert("model".to_owned(), target.model.clone().into());
        let mut appender = ParentedAppender {
            writer,
            bus: &mut self.bus,
            persisted_events: &mut self.persisted_events,
            session_id: &self.config.session_id,
            agent_id: &self.config.agent_id,
        };
        let event = appender.append(EventKind::AGENT_SPAWN, payload, None)?;
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
        // The task budget's max_tokens must bound the provider call itself,
        // not only the post-round accounting: a companion whose brief allows
        // 8192 tokens must not be silently capped at the provider default
        // because the parent session never set --max-output-tokens.
        let max_output_tokens = match (session.config.max_output_tokens, task.budget().max_tokens())
        {
            (Some(session_cap), Some(task_cap)) => Some(session_cap.min(task_cap)),
            (session_cap, task_cap) => session_cap.or(task_cap),
        };
        Self {
            session_id: session.config.session_id.clone(),
            agent_id,
            target,
            task,
            workspace_root: session.config.root.clone(),
            auto_compaction: session.config.auto_compaction,
            reasoning_effort: session.config.reasoning_effort,
            max_output_tokens,
            transport_retries: session.config.provider_transport_retries,
            transport_retry_backoff_ms: session.config.provider_transport_retry_backoff_ms.clone(),
            providers: &session.providers,
            tools: &session.tools,
            writer,
            bus: &mut session.bus,
            persisted_events: &mut session.persisted_events,
            permissions,
            turn_state: TurnState::default(),
            tool_calls: 0,
            tokens: 0,
        }
    }

    /// Companion rounds run through the shared [`RoundLoop`] seam, so
    /// companions inherit its transport retry (ADR 2026-07-06). max_turns
    /// maps onto the loop's round limit: it counts companion model rounds,
    /// and max_turns = 1 means at most one model round total.
    fn run(&mut self, cancel_flag: &AtomicBool) -> AgentResult {
        let config = RoundLoopConfig {
            max_rounds: self.task.budget().max_turns().map(|max| max as usize),
            transport_retries: self.transport_retries,
            transport_retry_backoff_ms: self.transport_retry_backoff_ms.clone(),
        };
        match RoundLoop::new(self, config).run(cancel_flag) {
            Ok(result) => result,
            Err(error) => companion_failure(error.to_string()),
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
                self.tools.root(),
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
                self.emit_tool_failure(call.id, call.name, error.to_string(), tool_call_event_id)?;
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
        let payload = object([
            ("path", patch.path.clone().into()),
            ("old", patch.before.clone().into()),
            ("new", patch.after.clone().into()),
        ]);
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
        self.append(
            EventKind::FILE_DIFF,
            file_diff_payload(&call.id, &file_change_id, patch),
            Some(patch_applied_id),
        )?;
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
            self.append(
                EventKind::FILE_DIFF,
                crate::file_diff::observed_file_diff_payload(
                    call_id,
                    &file_change_id,
                    "run_shell",
                    change,
                ),
                None,
            )?;
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
            ("output", execution.output.into()),
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
                ("error", error.into()),
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

    fn token_budget_exhausted(&self) -> bool {
        self.task
            .budget()
            .max_tokens()
            .is_some_and(|max| self.tokens > max)
    }

    fn add_usage(&mut self, usage: Option<&Usage>) {
        if let Some(usage) = usage {
            self.tokens = self.tokens.saturating_add(used_tokens(usage));
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
        let canvas = assemble_canvas(self.bus.events(), &self.auto_compaction);
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
        if let Some(max_output_tokens) = self.max_output_tokens {
            model_call.insert("max_output_tokens".to_owned(), max_output_tokens.into());
        }
        let model_call_id = self.append(EventKind::MODEL_CALL, model_call, None)?.id;
        let mut input = canvas.iter().map(model_input_item).collect::<Vec<_>>();
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
            max_output_tokens: self.max_output_tokens,
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
        let mut payload = object([
            ("source", "provider".into()),
            ("message", error.to_string().into()),
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
    fn append(
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

fn companion_success(content: String) -> AgentResult {
    if content.len() > euler_agents::MAX_OUTPUT_BYTES {
        return companion_failure("companion output exceeds 64KiB");
    }
    let output = (!content.is_empty()).then_some(content);
    AgentResult::success(COMPANION_SUCCESS_SUMMARY, output.as_deref())
        .expect("bounded companion success result should be valid")
}

fn companion_failure(error: impl AsRef<str>) -> AgentResult {
    AgentResult::failure(
        COMPANION_FAILURE_SUMMARY,
        error.as_ref(),
        Option::<&str>::None,
    )
    .expect("companion failure text should be bounded")
}

fn usage_payload(usage: Option<&Usage>) -> Value {
    match usage {
        Some(usage) => {
            let mut value = object([
                ("input_tokens", usage.input_tokens.into()),
                ("output_tokens", usage.output_tokens.into()),
            ]);
            if let Some(cached_tokens) = usage.cached_tokens {
                value.insert("cached_tokens".to_owned(), cached_tokens.into());
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
