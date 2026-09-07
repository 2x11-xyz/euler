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
            // The permissive danger walk (the second parser of
            // `command_safety`) vetoes every auto-approval path: a command
            // containing a forced `rm` anywhere — inside control flow, a
            // substitution, or a `sudo`/`env`/`trap`/`xargs` wrapper —
            // takes an explicit permission decision no matter what else
            // would have covered it, and `mode_for_request` escalates every
            // mode short of `always-deny` to an ask for it (ADR 0021
            // decision D). Under a never-prompt decider that path denies.
            // Walked once, when the request was built.
            let dangerous_command = request.dangerous_command;
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
                && !dangerous_command
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
            covered_grant_source =
                if mode == ApprovalMode::Ask && !static_safe && !dangerous_command {
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
                    let checkpoint =
                        match prepare_checkpoint(self.config.root.as_path(), &call.id, patch) {
                            Ok(checkpoint) => checkpoint,
                            Err(reason) => {
                                self.emit_failed_tool_result(
                                    call.id,
                                    execution.name,
                                    reason,
                                    tool_call_event_id,
                                    tool_started,
                                )?;
                                return Ok(());
                            }
                        };
                    let checkpoint_event_id = match &checkpoint {
                        Some(checkpoint) => Some(self.emit_with_parent(
                            EventKind::CHECKPOINT_STORED,
                            checkpoint.payload.clone(),
                            Some(patch_proposed_id.clone()),
                        )?),
                        None => None,
                    };
                    match self.tools.apply_patch_cancellable(patch, cancellation) {
                        Ok(()) => {}
                        Err(ToolError::Cancelled) => {
                            self.emit_cancelled_tool_result(
                                call,
                                tool_call_event_id,
                                Some(&execution),
                                Some(tool_started),
                            )?;
                            return Err(SessionError::Cancelled);
                        }
                        Err(error) => {
                            self.emit_failed_tool_result(
                                call.id,
                                execution.name,
                                error.to_string(),
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
                    let file_change_id = self.emit_with_parent(
                        EventKind::FILE_CHANGE,
                        file_change_payload(
                            &call.id,
                            patch,
                            checkpoint
                                .as_ref()
                                .map(|checkpoint| checkpoint.blob.as_str()),
                            checkpoint_event_id.as_deref(),
                        ),
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
        debug_assert_eq!(execution.name, "run_shell");
        let origin = "run_shell";
        for change in &execution.file_changes {
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
        ("ok", true.into()),
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
    // Reaching this builder means the executor returned a completed
    // ToolExecution. That is distinct from the process outcome: a nonzero
    // exit makes the canonical tool operation fail while its output and exit
    // code remain durable evidence.
    let succeeded = tool_result_succeeded(&payload);
    payload.insert("ok".to_owned(), succeeded.into());
    if !succeeded {
        let exit_code = execution.exit_code.unwrap_or(-1);
        debug_assert_ne!(exit_code, 0, "zero exit code must be successful");
        payload.insert(
            "error".to_owned(),
            format!("process exited with code {exit_code}").into(),
        );
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
    checkpoint_event_id: Option<&str>,
) -> JsonObject {
    let mut payload = object([
        ("tool_call_id", tool_call_id.to_owned().into()),
        ("origin", patch.origin.into()),
        ("action", patch.action.into()),
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
        if let Some(event_id) = checkpoint_event_id {
            payload.insert("checkpoint_event_id".to_owned(), event_id.into());
        }
    }
    payload
}

/// A rollback pre-image that has been made durable but whose write has not
/// happened yet.
pub(crate) struct PreparedCheckpoint {
    pub(crate) blob: String,
    pub(crate) payload: JsonObject,
}

/// Store the rollback pre-image for `patch` and build the `checkpoint.stored`
/// record for it (audit F36).
///
/// This is the only way to reach a destructive structured write: `Err` means
/// a checkpoint was owed but could not be made durable, and the caller must
/// abandon the write with the returned model-visible reason rather than
/// change a file it cannot undo. `Ok(None)` means no checkpoint is owed —
/// an add, or content deliberately not checkpointed.
pub(crate) fn prepare_checkpoint(
    root: &std::path::Path,
    tool_call_id: &str,
    patch: &PatchEvents,
) -> Result<Option<PreparedCheckpoint>, String> {
    // v0: modify-only. Adds have empty before; restore-as-delete is product debt.
    if patch.action != "modify" || patch.before.is_empty() {
        return Ok(None);
    }
    let blob =
        crate::checkpoints::store_pre_image(root, &patch.path, &patch.before).map_err(|error| {
            format!(
                "the rollback checkpoint for this edit could not be stored ({error}); \
the file was not changed"
            )
        })?;
    Ok(blob.map(|blob| PreparedCheckpoint {
        payload: object([
            ("tool_call_id", tool_call_id.to_owned().into()),
            ("path", patch.path.clone().into()),
            ("action", patch.action.into()),
            ("pre_image_blob", blob.as_str().into()),
            (
                "status",
                crate::checkpoints::CHECKPOINT_STATUS_PREPARED.into(),
            ),
        ]),
        blob,
    }))
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
