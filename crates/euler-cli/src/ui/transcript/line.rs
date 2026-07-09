pub(super) fn render_line_oriented_item(item: &super::TranscriptItem) -> String {
    match item {
        super::TranscriptItem::Banner { .. } => String::new(),
        super::TranscriptItem::TurnSeparator => String::new(),
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
        } => {
            format!("tool.result: {name} failed: {error}\n")
        }
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
        super::TranscriptItem::PermissionPrompt { capability, .. } => {
            format!("permission.prompt: {capability}\n")
        }
        super::TranscriptItem::PermissionAsk { capability, .. } => {
            format!("permission.ask: {capability}\n")
        }
        super::TranscriptItem::PermissionDecision { decision, .. } => {
            format!("permission.decision: {decision}\n")
        }
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
            if *ok {
                format!("check.result: {name} ok\n")
            } else {
                format!("check.result: {name} failed\n")
            }
        }
        super::TranscriptItem::SessionSummary(summary) => format!("session.summary: {summary}\n"),
        super::TranscriptItem::Interrupted => "interrupted\n".to_owned(),
        super::TranscriptItem::WorkedDuration(duration) => format!("worked: {duration}\n"),
        super::TranscriptItem::Error { source, message } => format!("error: {source}: {message}\n"),
    }
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
