//! Tool-call dispatch: the per-call permission braid entry, tool execution,
//! and the patch/file-change/diff emission it drives, plus the shared
//! failed-tool-result path and the patch/diff payload builders.
use super::{
    elapsed_ms, permission_request_for_tool, EventSink, PermissionRuling, Session, SessionError,
    TurnState,
};
use crate::file_diff::{
    file_diff_projection, observed_file_change_payload, observed_file_diff_payload, FileDiffSource,
};
use crate::permissions::{ApprovalMode, PermissionDecider};
use crate::redaction::SecretRedactor;
use crate::tools::{PatchEvents, ToolError, ToolExecution, ToolExecutionOutcome};
use euler_event::{object, tool_result_succeeded, EventEnvelope, EventKind, JsonObject};
use euler_provider::ToolCall;
use euler_sdk::{CancellationToken, Capability};
use serde_json::Value;
use std::time::Instant;

impl<D: PermissionDecider> Session<D> {
    pub(super) fn record_tool_call<F>(
        &mut self,
        call: &ToolCall,
        model_result_id: &str,
        sink: &mut EventSink<'_, F>,
    ) -> Result<String, SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        let mut payload = object([
            ("id", call.id.clone().into()),
            ("name", call.name.clone().into()),
            ("input", call.input.clone()),
        ]);
        if let Some((extension_id, command)) = self.extension_tool_attribution(&call.name) {
            payload.insert("extension_id".to_owned(), extension_id.into());
            payload.insert("command".to_owned(), command.into());
        }
        if matches!(call.name.as_str(), "run_shell" | "git_status" | "git_diff") {
            payload.insert(
                "workspace_authority".to_owned(),
                self.tools.workspace_authority_payload(),
            );
        }
        let tool_call_event_id = self.emit_with_parent(
            EventKind::TOOL_CALL,
            payload,
            Some(model_result_id.to_owned()),
        )?;
        self.flag_tool_call_exposure(&tool_call_event_id, &call.input)?;
        sink.flush(self.bus.events());
        Ok(tool_call_event_id)
    }

    #[allow(clippy::too_many_lines)] // ratchet: 188 lines, refactor target
    pub(super) fn execute_recorded_tool_call<F>(
        &mut self,
        call: ToolCall,
        tool_call_event_id: String,
        sink: &mut EventSink<'_, F>,
        turn_state: &mut TurnState,
        cancellation: &CancellationToken,
    ) -> Result<(), SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        if cancellation.is_cancelled() {
            self.emit_cancelled_tool_result(call, tool_call_event_id, None, None)?;
            return Err(SessionError::Cancelled);
        }
        if let Some(binding) = self.active_extension_tool(&call.name) {
            return self.execute_extension_model_tool(
                binding,
                call,
                tool_call_event_id,
                cancellation,
            );
        }
        let mut covered_grant_source: Option<crate::GrantSource> = None;
        let mut static_safe = false;
        if let Some(capability) = self
            .tools
            .required_capability_for_input(&call.name, &call.input)
        {
            if turn_state.denied(capability) {
                self.emit_permission_denied_tool_result(
                    call,
                    tool_call_event_id,
                    &format!(
                        "permission denied: {} was denied earlier this turn and \
                         remains denied for the rest of it — do not retry {} \
                         commands; use a different tool or ask the user",
                        capability.as_str(),
                        capability.as_str()
                    ),
                )?;
                return Ok(());
            }
            let request = permission_request_for_tool(
                capability,
                &self.tools.permission_reason(&call.name, &call.input),
                &call.name,
                &call.input,
                &self.tools,
            );
            // Request-aware mode: a sensitive-basename path escalates
            // blanket SessionAllow to Ask (deep review P1-b), so the grant
            // coverage and ask branches below apply to it like any other
            // uncovered request.
            let mode = self.permissions.mode_for_request(&request);
            // Statically-safe read-only shell commands run under `ask`
            // without a prompt (issue #78): recorded as a fresh
            // permission.decision with mode "static-safe" — allowed-once
            // semantics, no grant installed, parented to the tool call. The
            // check sits before grant coverage so the ledger attributes the
            // run to the analysis, not to an unrelated grant. It never
            // applies under always-deny, and a denial earlier this turn
            // still short-circuits above. A TRUNCATED command is never
            // analyzed: the bounded prefix could parse as safe while
            // `sh -c` runs the full string (security review, #66 class) —
            // decomposing a truncated command is decomposing a lie.
            static_safe = mode == ApprovalMode::Ask
                && capability == Capability::ShellExec
                && !request.command_truncated
                && request.command.as_deref().is_some_and(|command| {
                    crate::command_safety::is_statically_safe_command(command, self.tools.root())
                });
            if static_safe {
                self.emit_static_safe_decision(capability, tool_call_event_id.clone())?;
            }
            // A request covered by an existing session/project grant runs
            // under THAT decision: no prompt, and no fresh permission.decision
            // event — recording "allowed once" here would misstate what the
            // user actually granted (review v2 §8). The tool result carries a
            // `grant_source` tag so the ledger can show `· session grant`.
            covered_grant_source = if mode == ApprovalMode::Ask && !static_safe {
                self.permissions.granted_source(&request)
            } else {
                None
            };
            if covered_grant_source.is_none() && !static_safe {
                let ruling = self.decide_uncovered_permission(
                    &request,
                    &tool_call_event_id,
                    sink,
                    turn_state,
                    cancellation,
                );
                let ruling = match ruling {
                    Ok(ruling) => ruling,
                    Err(SessionError::Cancelled) => {
                        self.emit_cancelled_tool_result(call, tool_call_event_id, None, None)?;
                        return Err(SessionError::Cancelled);
                    }
                    Err(error) => return Err(error),
                };
                match ruling {
                    PermissionRuling::Allowed => {}
                    PermissionRuling::Denied { message } => {
                        self.emit_permission_denied_tool_result(
                            call,
                            tool_call_event_id,
                            &message,
                        )?;
                        return Ok(());
                    }
                }
            }
        }

        if cancellation.is_cancelled() {
            self.emit_cancelled_tool_result(call, tool_call_event_id, None, None)?;
            return Err(SessionError::Cancelled);
        }
        if call.name == super::swarm_tool::CODE_SWARM_REVIEW_TOOL {
            return self.execute_code_swarm_review_tool(
                call,
                tool_call_event_id,
                covered_grant_source,
                sink,
                cancellation,
            );
        }

        let tool_name = call.name.clone();
        let tool_started = Instant::now();
        match self.tools.execute_with_events_cancellable(
            &call.name,
            &call.input,
            self.bus.events(),
            cancellation,
        ) {
            Ok(ToolExecutionOutcome::Completed(execution)) => {
                // The input format was accepted: reset this tool's re-teach
                // streak even if a later write fails for environmental
                // reasons (the streak tracks format competence, issue #94).
                self.tool_reteach
                    .record_success(self.tools.reteach_identity(&call.name, &call.input));
                if let Some(patch) = execution.patch.as_ref() {
                    let mut payload = object([
                        (
                            "workspace_root",
                            patch.workspace_root.to_string_lossy().into_owned().into(),
                        ),
                        ("path", patch.path.clone().into()),
                        ("old", patch.before.clone().into()),
                        ("new", patch.after.clone().into()),
                    ]);
                    self.redactor
                        .redact_payload_fields(&mut payload, &["old", "new"]);
                    let patch_proposed_id = self.emit_with_parent(
                        EventKind::PATCH_PROPOSED,
                        payload.clone(),
                        Some(tool_call_event_id.clone()),
                    )?;
                    match self
                        .tools
                        .apply_patch_cancellable_observed(patch, cancellation)
                    {
                        Ok(()) => {}
                        Err(failure) if matches!(failure.error, ToolError::Cancelled) => {
                            self.emit_observed_changes(
                                &call.id,
                                patch.origin,
                                &failure.file_changes,
                                &tool_call_event_id,
                            )?;
                            self.emit_cancelled_tool_result(
                                call,
                                tool_call_event_id,
                                Some(&execution),
                                Some(tool_started),
                            )?;
                            return Err(SessionError::Cancelled);
                        }
                        Err(failure) => {
                            self.emit_observed_changes(
                                &call.id,
                                patch.origin,
                                &failure.file_changes,
                                &tool_call_event_id,
                            )?;
                            self.emit_failed_tool_result(
                                call.id,
                                execution.name,
                                failure.error.to_string(),
                                tool_call_event_id,
                                tool_started,
                            )?;
                            return Ok(());
                        }
                    }
                    let patch_applied_id = self.emit_with_parent(
                        EventKind::PATCH_APPLIED,
                        payload,
                        Some(patch_proposed_id),
                    )?;
                    let pre_image_blob = maybe_store_pre_image(patch);
                    let file_change_id = self.emit_with_parent(
                        EventKind::FILE_CHANGE,
                        file_change_payload(&call.id, patch, pre_image_blob.as_deref()),
                        Some(patch_applied_id.clone()),
                    )?;
                    let mut diff_payload = file_diff_payload(&call.id, &file_change_id, patch);
                    self.redactor
                        .redact_payload_fields(&mut diff_payload, &["diff"]);
                    self.emit_with_parent(
                        EventKind::FILE_DIFF,
                        diff_payload,
                        Some(patch_applied_id),
                    )?;
                }
                self.emit_observed_tool_changes(&call.id, &execution, &tool_call_event_id)?;
                let mut payload = tool_result_payload(call.id, &execution, &self.redactor);
                let succeeded = tool_result_succeeded(&payload);
                if let Some(source) = covered_grant_source {
                    // Ran under an existing grant — the ledger shows a dim
                    // `· session grant` on the tool header instead of a fresh
                    // decision record (review v2 §8).
                    payload.insert("grant_source".to_owned(), source.as_str().into());
                }
                if static_safe {
                    // Ran under static command-safety analysis — the ledger
                    // shows a dim `· safe` on the tool header (the decision
                    // record itself is suppressed like covered grants).
                    payload.insert("static_safe".to_owned(), true.into());
                }
                self.emit_with_parent(EventKind::TOOL_RESULT, payload, Some(tool_call_event_id))?;
                crate::diagnostics::tool_exec_end(
                    &self.config.session_id,
                    &tool_name,
                    elapsed_ms(tool_started),
                    succeeded,
                );
            }
            Ok(ToolExecutionOutcome::Cancelled(execution)) => {
                self.emit_cancelled_tool_result(
                    call,
                    tool_call_event_id,
                    Some(&execution),
                    Some(tool_started),
                )?;
                return Err(SessionError::Cancelled);
            }
            Err(ToolError::Cancelled) => {
                self.emit_cancelled_tool_result(
                    call,
                    tool_call_event_id,
                    None,
                    Some(tool_started),
                )?;
                return Err(SessionError::Cancelled);
            }
            Err(error) => {
                // Rung-2 re-teaching (issue #94): repeated consecutive
                // failures of a formatted tool append its full-format
                // payload to the error the model reads next.
                let error = self.tools.teach_on_failure(
                    &mut self.tool_reteach,
                    &call.name,
                    &call.input,
                    error.to_string(),
                );
                self.emit_failed_tool_result(
                    call.id,
                    call.name,
                    error,
                    tool_call_event_id,
                    tool_started,
                )?;
            }
        }
        Ok(())
    }

    fn emit_observed_tool_changes(
        &mut self,
        call_id: &str,
        execution: &ToolExecution,
        tool_call_event_id: &str,
    ) -> Result<(), SessionError> {
        if execution.file_changes.is_empty() {
            return Ok(());
        }
        debug_assert!(matches!(
            execution.name.as_str(),
            "run_shell" | "git_status" | "git_diff"
        ));
        self.emit_observed_changes(
            call_id,
            &execution.name,
            &execution.file_changes,
            tool_call_event_id,
        )
    }

    fn emit_observed_changes(
        &mut self,
        call_id: &str,
        origin: &str,
        changes: &[crate::ObservedFileChange],
        tool_call_event_id: &str,
    ) -> Result<(), SessionError> {
        for change in changes {
            let file_change_id = self.emit_with_parent(
                EventKind::FILE_CHANGE,
                observed_file_change_payload(call_id, origin, change),
                Some(tool_call_event_id.to_owned()),
            )?;
            let mut observed_diff =
                observed_file_diff_payload(call_id, &file_change_id, origin, change);
            self.redactor
                .redact_payload_fields(&mut observed_diff, &["diff"]);
            self.emit_with_parent(
                EventKind::FILE_DIFF,
                observed_diff,
                Some(tool_call_event_id.to_owned()),
            )?;
        }
        Ok(())
    }

    /// Close one admitted tool call after cancellation. Partial subprocess
    /// output and workspace effects are evidence, not success: emit them
    /// before the terminal failed result and mark the result explicitly.
    pub(super) fn emit_cancelled_tool_result(
        &mut self,
        call: ToolCall,
        tool_call_event_id: String,
        execution: Option<&ToolExecution>,
        tool_started: Option<Instant>,
    ) -> Result<(), SessionError> {
        if let Some(execution) = execution {
            self.emit_observed_tool_changes(&call.id, execution, &tool_call_event_id)?;
        }
        let payload = tool_cancelled_payload(call.id, call.name.clone(), execution, &self.redactor);
        self.emit_with_parent(EventKind::TOOL_RESULT, payload, Some(tool_call_event_id))?;
        if let Some(tool_started) = tool_started {
            crate::diagnostics::tool_exec_end(
                &self.config.session_id,
                &call.name,
                elapsed_ms(tool_started),
                false,
            );
        }
        Ok(())
    }

    /// Failed tool-result emission shared by the execution-error and
    /// patch-write-failure paths of [`Self::execute_recorded_tool_call`].
    fn emit_failed_tool_result(
        &mut self,
        call_id: String,
        name: String,
        error: String,
        tool_call_event_id: String,
        tool_started: Instant,
    ) -> Result<(), SessionError> {
        self.emit_with_parent(
            EventKind::TOOL_RESULT,
            object([
                ("id", call_id.into()),
                ("name", name.clone().into()),
                ("ok", false.into()),
                // Preserve the failed-error redaction main applies
                // (#67): a tool error may echo a secret-bearing arg.
                ("error", self.redactor.redact(&error).into()),
            ]),
            Some(tool_call_event_id),
        )?;
        crate::diagnostics::tool_exec_end(
            &self.config.session_id,
            &name,
            elapsed_ms(tool_started),
            false,
        );
        Ok(())
    }
}

pub(crate) fn tool_result_payload(
    call_id: String,
    execution: &ToolExecution,
    redactor: &SecretRedactor,
) -> JsonObject {
    let redacted_output = redactor.redact(&execution.output);
    let mut payload = object([
        ("id", call_id.into()),
        ("name", execution.name.clone().into()),
        ("ok", execution.failure.is_none().into()),
        ("output", redacted_output.clone().into()),
    ]);
    if let Some(budget) = execution.output_preview_budget {
        // Retain the producer's projection contract even when the current
        // output fits. A later explicit scrub can expand replacement markers;
        // the durable budget must still bound that rewritten result.
        payload.insert(
            "output_preview_max_bytes".to_owned(),
            budget.max_bytes.into(),
        );
        payload.insert(
            "output_preview_max_lines".to_owned(),
            budget.max_lines.into(),
        );
    }
    if let Some(digest) = &execution.project_context_snapshot_digest {
        payload.insert(
            "project_context_snapshot_digest".to_owned(),
            digest.clone().into(),
        );
    }
    if let Some(exit_code) = execution.exit_code {
        payload.insert("exit_code".to_owned(), exit_code.into());
    }
    if let Some(failure) = &execution.failure {
        payload.insert("failure_kind".to_owned(), failure.kind().into());
    }
    // Reaching this builder means the executor returned a completed
    // ToolExecution. That is distinct from the operation outcome: a nonzero
    // process exit or typed non-exit failure narrows `ok` while output and
    // compatibility status remain durable evidence.
    let succeeded = tool_result_succeeded(&payload);
    payload.insert("ok".to_owned(), succeeded.into());
    if !succeeded {
        let error = if let Some(failure) = &execution.failure {
            failure.error()
        } else {
            let exit_code = execution.exit_code.unwrap_or(-1);
            debug_assert_ne!(exit_code, 0, "zero exit code must be successful");
            payload.insert("failure_kind".to_owned(), "process-exit".into());
            format!("process exited with code {exit_code}")
        };
        payload.insert("error".to_owned(), redactor.redact(&error).into());
    }
    payload
}

pub(crate) fn tool_cancelled_payload(
    call_id: String,
    name: String,
    execution: Option<&ToolExecution>,
    redactor: &SecretRedactor,
) -> JsonObject {
    let mut payload = object([
        ("id", call_id.into()),
        ("name", name.into()),
        ("ok", false.into()),
        ("error", "tool cancelled".into()),
        ("failure_kind", "cancelled".into()),
        ("cancelled", true.into()),
    ]);
    if let Some(execution) = execution {
        payload.insert(
            "output".to_owned(),
            redactor.redact(&execution.output).into(),
        );
        if let Some(exit_code) = execution.exit_code {
            payload.insert("exit_code".to_owned(), exit_code.into());
        }
        if let Some(budget) = execution.output_preview_budget {
            payload.insert(
                "output_preview_max_bytes".to_owned(),
                budget.max_bytes.into(),
            );
            payload.insert(
                "output_preview_max_lines".to_owned(),
                budget.max_lines.into(),
            );
        }
        if let Some(digest) = &execution.project_context_snapshot_digest {
            payload.insert(
                "project_context_snapshot_digest".to_owned(),
                digest.clone().into(),
            );
        }
    }
    payload
}

pub(crate) fn file_change_payload(
    tool_call_id: &str,
    patch: &PatchEvents,
    pre_image_blob: Option<&str>,
) -> JsonObject {
    let mut payload = object([
        ("tool_call_id", tool_call_id.to_owned().into()),
        ("origin", patch.origin.into()),
        ("action", patch.action.into()),
        (
            "workspace_root",
            patch.workspace_root.to_string_lossy().into_owned().into(),
        ),
        ("path", patch.path.clone().into()),
        ("old_path", Value::Null),
        (
            "before_sha256",
            patch
                .before_sha256
                .as_ref()
                .map_or(Value::Null, |sha| sha.clone().into()),
        ),
        ("after_sha256", patch.after_sha256.clone().into()),
        ("before_byte_len", patch.before_byte_len.into()),
        ("after_byte_len", patch.after_byte_len.into()),
        ("diff_redaction", "omitted".into()),
    ]);
    if let Some(hash) = pre_image_blob {
        payload.insert("pre_image_blob".to_owned(), hash.into());
    }
    payload
}

pub(crate) fn maybe_store_pre_image(patch: &PatchEvents) -> Option<String> {
    // v0: modify-only. Adds have empty before; restore-as-delete is product debt.
    if patch.action != "modify" || patch.before.is_empty() {
        return None;
    }
    crate::checkpoints::store_pre_image(&patch.workspace_root, &patch.path, &patch.before)
}

pub(crate) fn file_diff_payload(
    tool_call_id: &str,
    file_change_id: &str,
    patch: &PatchEvents,
) -> JsonObject {
    let projection = file_diff_projection(FileDiffSource {
        path: &patch.path,
        action: patch.action,
        before: &patch.before,
        after: &patch.after,
    });
    object([
        ("tool_call_id", tool_call_id.to_owned().into()),
        ("file_change_id", file_change_id.to_owned().into()),
        (
            "workspace_root",
            patch.workspace_root.to_string_lossy().into_owned().into(),
        ),
        ("path", patch.path.clone().into()),
        ("old_path", Value::Null),
        ("action", patch.action.into()),
        ("origin", patch.origin.into()),
        (
            "diff",
            projection
                .diff
                .map_or(Value::Null, std::convert::Into::into),
        ),
        ("truncated", projection.truncated.into()),
        ("truncation", projection.truncation.into()),
        (
            "omitted_reason",
            projection
                .omitted_reason
                .map_or(Value::Null, std::convert::Into::into),
        ),
    ])
}

#[cfg(test)]
mod outcome_tests {
    use super::*;
    use crate::canvas::{assemble_canvas, AutoCompactionPolicy};
    use crate::tools::ToolExecutionFailure;

    fn execution(exit_code: i32, failure: Option<ToolExecutionFailure>) -> ToolExecution {
        ToolExecution {
            name: "run_shell".to_owned(),
            output: "collected output".to_owned(),
            output_preview_budget: None,
            project_context_snapshot_digest: None,
            exit_code: Some(exit_code),
            failure,
            patch: None,
            file_changes: Vec::new(),
        }
    }

    #[test]
    fn typed_non_exit_failures_never_claim_a_normal_process_exit() {
        let cases = [
            (
                ToolExecutionFailure::TimedOut { timeout_ms: 250 },
                "timeout",
                "command timed out after 250 ms and its process group was killed",
            ),
            (
                ToolExecutionFailure::Signaled { signal: 15 },
                "signal",
                "command terminated by signal 15",
            ),
            (
                ToolExecutionFailure::AbnormalTermination,
                "abnormal-termination",
                "command terminated abnormally without an exit code",
            ),
            (
                ToolExecutionFailure::SupervisionFailed,
                "supervision-failed",
                "command supervision failed after launch; its process group was killed",
            ),
            (
                ToolExecutionFailure::WorkspaceMutation {
                    tool_name: "git_diff".to_owned(),
                },
                "workspace-mutation",
                "workspace mutation was observed during git_diff; the Git view was invalidated",
            ),
        ];

        for (failure, kind, error) in cases {
            let payload = tool_result_payload(
                "call".to_owned(),
                &execution(-1, Some(failure)),
                &SecretRedactor::new(),
            );
            assert_eq!(payload.get("ok"), Some(&Value::Bool(false)));
            assert_eq!(
                payload.get("failure_kind").and_then(Value::as_str),
                Some(kind)
            );
            assert_eq!(payload.get("error").and_then(Value::as_str), Some(error));
            assert_eq!(
                payload.get("output").and_then(Value::as_str),
                Some("collected output")
            );
            assert_eq!(payload.get("exit_code").and_then(Value::as_i64), Some(-1));
            assert!(!error.contains("exited with code"));
        }
    }

    #[test]
    fn ordinary_nonzero_exit_remains_a_process_exit_failure() {
        let payload = tool_result_payload(
            "call".to_owned(),
            &execution(101, None),
            &SecretRedactor::new(),
        );

        assert_eq!(payload.get("ok"), Some(&Value::Bool(false)));
        assert_eq!(
            payload.get("failure_kind").and_then(Value::as_str),
            Some("process-exit")
        );
        assert_eq!(
            payload.get("error").and_then(Value::as_str),
            Some("process exited with code 101")
        );
    }

    #[test]
    fn signal_failure_reaches_model_input_without_becoming_a_process_exit() {
        let call = EventEnvelope::new(
            "session",
            "agent",
            None,
            EventKind::TOOL_CALL,
            object([
                ("id", "call-signal".into()),
                ("name", "run_shell".into()),
                ("input", serde_json::json!({"command": "fixture"})),
            ]),
        );
        let result = EventEnvelope::new(
            "session",
            "agent",
            Some(call.id.clone()),
            EventKind::TOOL_RESULT,
            tool_result_payload(
                "call-signal".to_owned(),
                &execution(-1, Some(ToolExecutionFailure::Signaled { signal: 15 })),
                &SecretRedactor::new(),
            ),
        );
        let canvas = assemble_canvas(&[call, result], &AutoCompactionPolicy::default());
        let input = canvas
            .iter()
            .map(super::super::model_input_item)
            .find(|item| matches!(item, euler_provider::ModelInputItem::ToolOutput { .. }))
            .expect("model-facing tool output");

        assert!(matches!(
            input,
            euler_provider::ModelInputItem::ToolOutput {
                ok: false,
                exit_code: Some(-1),
                error: Some(error),
                ..
            } if error == "command terminated by signal 15"
        ));
    }
}
