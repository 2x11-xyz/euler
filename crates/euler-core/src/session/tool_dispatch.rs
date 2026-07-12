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
use crate::tools::PatchEvents;
use euler_event::{object, EventEnvelope, EventKind, JsonObject};
use euler_provider::ToolCall;
use euler_sdk::Capability;
use serde_json::Value;
use std::time::Instant;

impl<D: PermissionDecider> Session<D> {
    #[allow(clippy::too_many_lines)] // ratchet: 188 lines, refactor target
    pub(super) fn execute_tool_call<F>(
        &mut self,
        call: ToolCall,
        model_result_id: String,
        sink: &mut EventSink<'_, F>,
        turn_state: &mut TurnState,
    ) -> Result<(), SessionError>
    where
        F: FnMut(&EventEnvelope),
    {
        let tool_call_event_id = self.emit_with_parent(
            EventKind::TOOL_CALL,
            object([
                ("id", call.id.clone().into()),
                ("name", call.name.clone().into()),
                ("input", call.input.clone()),
            ]),
            Some(model_result_id),
        )?;
        sink.flush(self.bus.events());

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
            let mode = self.permissions.mode(capability);
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
                match self.decide_uncovered_permission(
                    &request,
                    mode,
                    &tool_call_event_id,
                    sink,
                    turn_state,
                )? {
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

        if call.name == super::swarm_tool::CODE_SWARM_REVIEW_TOOL {
            return self.execute_code_swarm_review_tool(
                call,
                tool_call_event_id,
                covered_grant_source,
                sink,
            );
        }

        let tool_name = call.name.clone();
        let tool_started = Instant::now();
        match self
            .tools
            .execute_with_events(&call.name, &call.input, self.bus.events())
        {
            Ok(execution) => {
                // The input format was accepted: reset this tool's re-teach
                // streak even if a later write fails for environmental
                // reasons (the streak tracks format competence, issue #94).
                self.tool_reteach
                    .record_success(self.tools.reteach_identity(&call.name, &call.input));
                if let Some(patch) = execution.patch {
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
                    if let Err(error) = self.tools.apply_patch(&patch) {
                        self.emit_failed_tool_result(
                            call.id,
                            execution.name,
                            error.to_string(),
                            tool_call_event_id,
                            tool_started,
                        )?;
                        return Ok(());
                    }
                    let patch_applied_id = self.emit_with_parent(
                        EventKind::PATCH_APPLIED,
                        payload,
                        Some(patch_proposed_id),
                    )?;
                    let pre_image_blob = maybe_store_pre_image(self.config.root.as_path(), &patch);
                    let file_change_id = self.emit_with_parent(
                        EventKind::FILE_CHANGE,
                        file_change_payload(&call.id, &patch, pre_image_blob.as_deref()),
                        Some(patch_applied_id.clone()),
                    )?;
                    let mut diff_payload = file_diff_payload(&call.id, &file_change_id, &patch);
                    self.redactor
                        .redact_payload_fields(&mut diff_payload, &["diff"]);
                    self.emit_with_parent(
                        EventKind::FILE_DIFF,
                        diff_payload,
                        Some(patch_applied_id),
                    )?;
                }
                for change in &execution.file_changes {
                    let file_change_id = self.emit_with_parent(
                        EventKind::FILE_CHANGE,
                        observed_file_change_payload(&call.id, "run_shell", change),
                        Some(tool_call_event_id.clone()),
                    )?;
                    let mut observed_diff =
                        observed_file_diff_payload(&call.id, &file_change_id, "run_shell", change);
                    self.redactor
                        .redact_payload_fields(&mut observed_diff, &["diff"]);
                    self.emit_with_parent(
                        EventKind::FILE_DIFF,
                        observed_diff,
                        Some(tool_call_event_id.clone()),
                    )?;
                }
                let mut payload = object([
                    ("id", call.id.into()),
                    ("name", execution.name.into()),
                    ("ok", true.into()),
                    ("output", self.redactor.redact(&execution.output).into()),
                ]);
                if let Some(exit_code) = execution.exit_code {
                    payload.insert("exit_code".to_owned(), exit_code.into());
                }
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
                    true,
                );
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

    /// Failed tool-result emission shared by the execution-error and
    /// patch-write-failure paths of [`Self::execute_tool_call`].
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

pub(crate) fn file_change_payload(
    tool_call_id: &str,
    patch: &PatchEvents,
    pre_image_blob: Option<&str>,
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
    }
    payload
}

pub(crate) fn maybe_store_pre_image(root: &std::path::Path, patch: &PatchEvents) -> Option<String> {
    // v0: modify-only. Adds have empty before; restore-as-delete is product debt.
    if patch.action != "modify" || patch.before.is_empty() {
        return None;
    }
    crate::checkpoints::store_pre_image(root, &patch.path, &patch.before)
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
