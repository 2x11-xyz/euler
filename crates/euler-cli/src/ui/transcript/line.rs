pub(super) fn render_line_oriented_item(item: &super::TranscriptItem) -> String {
    match item {
        super::TranscriptItem::Banner { .. }
        | super::TranscriptItem::TurnSeparator
        | super::TranscriptItem::ModelReasoningLive { .. } => String::new(),
        super::TranscriptItem::UserMessage(content) => format!("user: {content}\n"),
        super::TranscriptItem::AssistantMessage(content) => format!("assistant: {content}\n"),
        super::TranscriptItem::AssistantActivity(content) => {
            format!("assistant.activity: {content}\n")
        }
        super::TranscriptItem::PlanUpdate(summary) => format!("plan.update: {summary}\n"),
        super::TranscriptItem::ModelCall { provider, model } => {
            format!("model.call: {provider}/{model}\n")
        }
        super::TranscriptItem::ModelResult(content) => format!("model.result: {content}\n"),
        super::TranscriptItem::ModelReasoning { fidelity, content } => {
            format!("model.reasoning: {fidelity}: {content}\n")
        }
        super::TranscriptItem::ToolCall { name } => format!("tool.call: {name}\n"),
        super::TranscriptItem::ToolResult { name, ok: true, .. } => {
            format!("tool.result: {name} ok\n")
        }
        super::TranscriptItem::ToolResult {
            name,
            ok: false,
            error,
            ..
        } => format!("tool.result: {name} failed: {error}\n"),
        super::TranscriptItem::ToolRun {
            command, ok: true, ..
        } => format!("tool.result: run_shell ok: {command}\n"),
        super::TranscriptItem::ToolRun {
            command,
            ok: false,
            error,
            ..
        } => format!("tool.result: run_shell failed: {command}: {error}\n"),
        super::TranscriptItem::Exploration { summaries } => {
            format!("explored: {}\n", summaries.join(", "))
        }
        super::TranscriptItem::PermissionPrompt { .. }
        | super::TranscriptItem::PermissionAsk { .. }
        | super::TranscriptItem::PermissionBatchAsk { .. }
        | super::TranscriptItem::PermissionDecision { .. } => line_oriented_permission(item),
        // The acknowledgment card is TUI-only live chrome, never a line-mode
        // ledger row.
        super::TranscriptItem::ProjectContextAck { .. } => String::new(),
        super::TranscriptItem::PatchProposed { path, old, new } => {
            line_oriented_patch("patch.proposed", path, old.as_deref(), new.as_deref())
        }
        super::TranscriptItem::PatchApplied { path, old, new } => {
            line_oriented_patch("patch.applied", path, old.as_deref(), new.as_deref())
        }
        super::TranscriptItem::FileChange { path, action, .. } => {
            line_oriented_file_change(path, action)
        }
        super::TranscriptItem::FileDiff {
            path, action, diff, ..
        } => line_oriented_file_diff(path, action, diff.as_deref()),
        super::TranscriptItem::WorkspaceRestore {
            path,
            checkpoint_event_id,
        } => format!("workspace.restore: {path} → ckpt {checkpoint_event_id}\n"),
        super::TranscriptItem::CheckStarted { name } => format!("check.started: {name}\n"),
        super::TranscriptItem::CheckResult { name, ok, .. } => {
            line_oriented_check_result(name, *ok)
        }
        super::TranscriptItem::SessionSummary(summary) => format!("session.summary: {summary}\n"),
        super::TranscriptItem::ExtensionResult { .. } => line_oriented_extension_result(item),
        super::TranscriptItem::Interrupted => "interrupted\n".to_owned(),
        super::TranscriptItem::WorkedDuration(duration) => format!("worked: {duration}\n"),
        super::TranscriptItem::TurnRecap { summary, files } => {
            line_oriented_turn_recap(summary, files.as_deref())
        }
        super::TranscriptItem::ResumeBoundary { .. } => line_oriented_resume_boundary(item),
        super::TranscriptItem::Companion { .. } => line_oriented_companion(item),
        super::TranscriptItem::Error { source, message } => format!("error: {source}: {message}\n"),
        super::TranscriptItem::Notice(message) => format!("notice: {message}\n"),
    }
}

fn line_oriented_permission(item: &super::TranscriptItem) -> String {
    match item {
        super::TranscriptItem::PermissionPrompt {
            capability,
            capabilities,
            operation,
            ..
        } => {
            let operation = operation.as_deref().filter(|value| !value.is_empty());
            let subject = operation.unwrap_or(capability.as_str());
            if operation.is_some() || !capabilities.is_empty() {
                let requested = if capabilities.is_empty() {
                    capability.clone()
                } else {
                    capabilities.join(", ")
                };
                format!("permission.prompt: {subject}; capabilities: {requested}\n")
            } else {
                format!("permission.prompt: {capability}\n")
            }
        }
        super::TranscriptItem::PermissionAsk { capability, .. } => {
            format!("permission.ask: {capability}\n")
        }
        super::TranscriptItem::PermissionBatchAsk {
            operation,
            capabilities,
            ..
        } => {
            format!(
                "permission.ask: {operation}; capabilities: {}\n",
                capabilities.join(", ")
            )
        }
        super::TranscriptItem::PermissionDecision { decision, .. } => {
            format!("permission.decision: {decision}\n")
        }
        _ => String::new(),
    }
}

fn line_oriented_turn_recap(summary: &str, files: Option<&str>) -> String {
    match files {
        Some(files) => format!("turn.recap: {summary}\nturn.recap.files: {files}\n"),
        None => format!("turn.recap: {summary}\n"),
    }
}

fn line_oriented_companion(item: &super::TranscriptItem) -> String {
    let super::TranscriptItem::Companion {
        name,
        task,
        status,
        rows,
        ..
    } = item
    else {
        return String::new();
    };
    let mut out = match status {
        super::CompanionStatus::Running { elapsed } => {
            let elapsed = elapsed
                .as_deref()
                .map(|value| format!(" · {value}"))
                .unwrap_or_default();
            format!("companion: {name} running · {task}{elapsed}\n")
        }
        super::CompanionStatus::Done {
            ok,
            summary,
            elapsed,
        } => {
            let state = if *ok { "done" } else { "failed" };
            let elapsed = elapsed
                .as_deref()
                .map(|value| format!(" {value}"))
                .unwrap_or_default();
            format!("companion: {name} {state}{elapsed} · {summary}\n")
        }
    };
    for row in rows {
        match row {
            super::CompanionRow::Finding { label, detail } => {
                out.push_str(&format!("  finding [{label}]: {detail}\n"));
            }
            super::CompanionRow::Report { text } => {
                out.push_str(&format!("  report: {text}\n"));
            }
        }
    }
    out
}

fn line_oriented_check_result(name: &str, ok: bool) -> String {
    if ok {
        format!("check.result: {name} ok\n")
    } else {
        format!("check.result: {name} failed\n")
    }
}

fn line_oriented_resume_boundary(item: &super::TranscriptItem) -> String {
    let super::TranscriptItem::ResumeBoundary {
        label,
        recovery_closure_appended,
        warning_count,
        events_replayed,
    } = item
    else {
        return String::new();
    };
    let decision = super::cells::resume_boundary_decision_text(
        label,
        *recovery_closure_appended,
        *warning_count,
    );
    format!(
        "{decision}\n──── {events_replayed} events replayed · model context folded to stubs ────\n"
    )
}

fn line_oriented_patch(label: &str, path: &str, old: Option<&str>, new: Option<&str>) -> String {
    format!("{label}: {}: {path}\n", super::patch_diff::action(old, new))
}

fn line_oriented_file_change(path: &str, action: &str) -> String {
    format!(
        "file.change: {}: {}\n",
        super::file_change_action_label(action),
        super::file_change_path_label(path)
    )
}

fn line_oriented_file_diff(path: &str, action: &str, diff: Option<&str>) -> String {
    let suffix = if diff.is_some() { "" } else { " (omitted)" };
    format!(
        "file.diff: {}: {}{suffix}\n",
        super::file_change_action_label(action),
        super::file_change_path_label(path)
    )
}

fn line_oriented_extension_result(item: &super::TranscriptItem) -> String {
    let super::TranscriptItem::ExtensionResult { reference, ok, .. } = item else {
        return String::new();
    };
    format!(
        "extension.result: {reference} {}\n",
        if *ok { "ok" } else { "failed" }
    )
}

#[cfg(test)]
mod tests {
    use super::render_line_oriented_item;
    use crate::ui::{patch_approval::ApprovalOption, transcript::TranscriptItem};

    #[test]
    fn batched_permission_ask_lists_the_requested_capabilities() {
        let item = TranscriptItem::PermissionBatchAsk {
            operation: "extension session-export.export".to_owned(),
            capabilities: vec![
                "fs-read".to_owned(),
                "fs-write".to_owned(),
                "network".to_owned(),
            ],
            selected_option: ApprovalOption::AllowOnce,
        };

        assert_eq!(
            render_line_oriented_item(&item),
            "permission.ask: extension session-export.export; capabilities: fs-read, fs-write, network\n"
        );
    }
}
