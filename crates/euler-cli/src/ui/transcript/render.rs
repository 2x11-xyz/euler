use super::cells::{
    edit_failure_status, output_rows_without_trailing_blanks, push_bounded_children,
    push_bounded_failure_children, push_cell_parent, push_child_rows, render_edit_cell,
    render_file_change_cell, render_interrupted, render_patch_cell, render_permission_ask,
    render_permission_decision, render_tool_run, render_worked_duration, tool_failure_status,
    EditRender, FileChangeRender, PatchRender, PermissionAskView, ToolRunRender,
};
use super::file_diff::{render_file_diff_cell, FileDiffRender};
use super::{EventTiming, ProjectedEntry, TranscriptItem, TOOL_CALL_MAX_LINES};
use crate::ui::glyphs::user_line_prefix;
use crate::ui::markdown;
use crate::ui::text::{
    blank_gutter, content_width, display_width, hairline_content, timestamp_gutter,
    tree_gutter_pipe, wrap_text, GUTTER_WIDTH,
};
use crate::ui::theme::Theme;
use ratatui::text::{Line, Span};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TranscriptRenderLimits {
    output_lines: usize,
    patch_detail_lines: usize,
}

impl Default for TranscriptRenderLimits {
    fn default() -> Self {
        Self {
            output_lines: TOOL_CALL_MAX_LINES,
            patch_detail_lines: super::super::patch_diff::DIFF_PREVIEW_ROWS
                .max(super::super::patch_diff::NEW_FILE_PREVIEW_ROWS)
                + 1,
        }
    }
}

impl TranscriptRenderLimits {
    pub(super) fn with_output_lines(mut self, output_lines: usize) -> Self {
        self.output_lines = output_lines;
        self
    }

    fn expanded(mut self) -> Self {
        self.output_lines = usize::MAX;
        self.patch_detail_lines = usize::MAX;
        self
    }
}

pub(super) fn render_projected_items(
    items: &[TranscriptItem],
    theme: &Theme,
    width: u16,
    limits: TranscriptRenderLimits,
) -> Vec<Line<'static>> {
    render_projected_items_with_expansion(items, theme, width, limits, &HashSet::new())
}

pub(super) fn render_projected_items_with_expansion(
    items: &[TranscriptItem],
    theme: &Theme,
    width: u16,
    limits: TranscriptRenderLimits,
    expanded_artifact_keys: &HashSet<String>,
) -> Vec<Line<'static>> {
    let entries: Vec<_> = items
        .iter()
        .cloned()
        .map(|item| ProjectedEntry { item, timing: None })
        .collect();
    render_projected_entries_with_expansion(&entries, theme, width, limits, expanded_artifact_keys)
}

pub(super) fn render_projected_entries(
    entries: &[ProjectedEntry],
    theme: &Theme,
    width: u16,
    limits: TranscriptRenderLimits,
) -> Vec<Line<'static>> {
    render_projected_entries_with_expansion(entries, theme, width, limits, &HashSet::new())
}

#[allow(clippy::too_many_lines)] // ratchet: ledger projection match, refactor target
pub(super) fn render_projected_entries_with_expansion(
    entries: &[ProjectedEntry],
    theme: &Theme,
    width: u16,
    limits: TranscriptRenderLimits,
    expanded_artifact_keys: &HashSet<String>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let content_cols = content_width(width);

    for (index, entry) in entries.iter().enumerate() {
        let first_line = lines.len();
        let item = &entry.item;
        let item_limits = if expanded_artifact_keys.contains(&super::artifact_key_for_index(index))
        {
            limits.expanded()
        } else {
            limits
        };
        match item {
            TranscriptItem::Banner { session_id } => {
                lines.extend(super::super::banner::styled_lines_with_session(
                    theme,
                    session_id.as_deref(),
                ));
            }
            TranscriptItem::TurnSeparator => {
                lines.push(Line::from(Span::styled(
                    "─".repeat(usize::from(width)),
                    theme.transcript.muted,
                )));
            }
            TranscriptItem::UserMessage(content) => {
                push_wrapped_with_continuation(
                    &mut lines,
                    (user_line_prefix(true), user_line_prefix(false)),
                    content,
                    theme.transcript.user,
                    theme,
                    width,
                );
            }
            TranscriptItem::AssistantMessage(content) => {
                lines.extend(render_assistant_prose(content, theme, width));
            }
            TranscriptItem::AssistantActivity(content) => {
                push_cell_parent(&mut lines, content, theme.transcript.control, theme, width);
            }
            TranscriptItem::PlanUpdate(summary) => {
                push_cell_parent(
                    &mut lines,
                    &format!("Updated Plan: {summary}"),
                    theme.transcript.control,
                    theme,
                    width,
                );
            }
            TranscriptItem::ModelCall { provider, model } => {
                push_wrapped(
                    &mut lines,
                    blank_gutter(),
                    &format!("* Model {provider}/{model}"),
                    theme.transcript.model,
                    theme,
                    width,
                );
            }
            TranscriptItem::ModelResult(content) => {
                push_wrapped(
                    &mut lines,
                    blank_gutter(),
                    &format!("* Model result: {content}"),
                    theme.transcript.model,
                    theme,
                    width,
                );
            }
            TranscriptItem::ModelReasoning { fidelity, content } => {
                push_wrapped(
                    &mut lines,
                    blank_gutter(),
                    &reasoning_summary(fidelity, content),
                    theme.transcript.reasoning,
                    theme,
                    width,
                );
            }
            TranscriptItem::ToolCall { name } => {
                push_wrapped(
                    &mut lines,
                    blank_gutter(),
                    &format!("* Tool {name}"),
                    theme.transcript.tool,
                    theme,
                    width,
                );
            }
            TranscriptItem::ToolResult {
                name,
                ok,
                error,
                output,
                exit_code,
                path,
            } => {
                let (heading, style) = if *ok {
                    (tool_result_label(name), theme.transcript.tool)
                } else if matches!(name.as_str(), "edit_file" | "apply_patch" | "apply-patch") {
                    (
                        edit_failure_status(path.as_deref().unwrap_or(""), error),
                        theme.transcript.tool_error,
                    )
                } else {
                    let label = tool_result_label(name);
                    let status = tool_failure_status(*exit_code, error);
                    (format!("{label} {status}"), theme.transcript.tool_error)
                };
                push_cell_parent(&mut lines, &heading, style, theme, width);
                if *ok {
                    push_bounded_children(
                        &mut lines,
                        output,
                        theme.transcript.muted,
                        theme,
                        width,
                        item_limits.output_lines,
                    );
                } else {
                    push_bounded_failure_children(
                        &mut lines,
                        output,
                        theme.transcript.muted,
                        theme,
                        width,
                        item_limits.output_lines,
                    );
                }
            }
            TranscriptItem::ToolRun {
                command,
                ok,
                error,
                output,
                exit_code,
            } => {
                render_tool_run(
                    &mut lines,
                    ToolRunRender {
                        command,
                        ok: *ok,
                        error,
                        output,
                        exit_code: *exit_code,
                    },
                    theme,
                    width,
                    item_limits.output_lines,
                );
            }
            TranscriptItem::Exploration { summaries } => {
                push_cell_parent(&mut lines, "explore", theme.transcript.tool, theme, width);
                push_child_rows(
                    &mut lines,
                    &super::coalesced_exploration_summaries(summaries),
                    theme.transcript.muted,
                    theme,
                    width,
                );
            }
            TranscriptItem::PermissionPrompt { capability, reason } => {
                let text = if reason.is_empty() {
                    format!("* Permission required: {capability}")
                } else {
                    format!("* Permission required: {capability} - {reason}")
                };
                push_wrapped(
                    &mut lines,
                    blank_gutter(),
                    &text,
                    theme.transcript.permission,
                    theme,
                    width,
                );
            }
            TranscriptItem::PermissionAsk {
                capability,
                reason,
                command,
                scope_prefix,
            } => {
                render_permission_ask(
                    &mut lines,
                    PermissionAskView {
                        capability,
                        reason,
                        command: command.as_deref(),
                        scope_prefix: scope_prefix.as_deref(),
                    },
                    theme,
                    width,
                );
            }
            TranscriptItem::PermissionDecision {
                capability,
                decision,
                allowed,
            } => {
                render_permission_decision(
                    &mut lines, capability, decision, *allowed, theme, width,
                );
            }
            TranscriptItem::PatchProposed { path, old, new } => {
                render_patch_cell(
                    &mut lines,
                    PatchRender {
                        label: "Patch proposed",
                        title: format!("Patch proposed {path}"),
                        path,
                        old: old.as_deref(),
                        new: new.as_deref(),
                    },
                    theme,
                    width,
                    item_limits.patch_detail_lines,
                );
            }
            TranscriptItem::PatchApplied { path, old, new } => {
                render_edit_cell(
                    &mut lines,
                    EditRender {
                        path,
                        old: old.as_deref(),
                        new: new.as_deref(),
                    },
                    theme,
                    width,
                    item_limits.patch_detail_lines,
                );
            }
            TranscriptItem::FileChange {
                path,
                action,
                origin,
                before_sha256,
                after_sha256,
                before_byte_len,
                after_byte_len,
                diff_redaction,
                checkpoint_event_id,
            } => {
                render_file_change_cell(
                    &mut lines,
                    FileChangeRender {
                        path,
                        action,
                        origin,
                        before_sha256: before_sha256.as_deref(),
                        after_sha256: after_sha256.as_deref(),
                        before_byte_len: *before_byte_len,
                        after_byte_len: *after_byte_len,
                        diff_redaction,
                        checkpoint_event_id: checkpoint_event_id.as_deref(),
                    },
                    theme,
                    width,
                );
            }
            TranscriptItem::FileDiff {
                path,
                action,
                origin,
                diff,
                truncated,
                truncation,
                omitted_reason,
                checkpoint_event_id,
            } => {
                render_file_diff_cell(
                    &mut lines,
                    FileDiffRender {
                        path,
                        action,
                        origin,
                        diff: diff.as_deref(),
                        truncated: *truncated,
                        truncation,
                        omitted_reason: omitted_reason.as_deref(),
                        checkpoint_event_id: checkpoint_event_id.as_deref(),
                    },
                    theme,
                    width,
                    item_limits.output_lines,
                );
            }
            TranscriptItem::WorkspaceRestore {
                path,
                checkpoint_event_id,
            } => {
                push_wrapped(
                    &mut lines,
                    blank_gutter(),
                    &format!(
                        "↩ reverted {path} → ckpt {checkpoint_event_id} · files restored, history intact"
                    ),
                    theme.transcript.muted,
                    theme,
                    width,
                );
            }
            TranscriptItem::CheckStarted { name } => {
                push_wrapped(
                    &mut lines,
                    blank_gutter(),
                    &format!("* Check started: {name}"),
                    theme.transcript.check,
                    theme,
                    width,
                );
            }
            TranscriptItem::CheckResult { name, ok, output } => {
                let status = if *ok { "passed" } else { "failed" };
                let style = if *ok {
                    theme.transcript.check
                } else {
                    theme.transcript.error
                };
                push_wrapped(
                    &mut lines,
                    blank_gutter(),
                    &format!("* Check {name} {status}"),
                    style,
                    theme,
                    width,
                );
                push_bounded_detail(
                    &mut lines,
                    output,
                    DetailRender {
                        style: theme.transcript.muted,
                        gutter: tree_gutter_pipe(),
                    },
                    theme,
                    width,
                    item_limits.output_lines,
                );
            }
            TranscriptItem::SessionSummary(summary) => {
                push_wrapped(
                    &mut lines,
                    blank_gutter(),
                    &format!("* Summary: {summary}"),
                    theme.transcript.control,
                    theme,
                    width,
                );
            }
            TranscriptItem::Interrupted => {
                render_interrupted(&mut lines, theme, width);
            }
            TranscriptItem::WorkedDuration(duration) => {
                render_worked_duration(&mut lines, duration, theme, width);
            }
            TranscriptItem::Error { source, message } => {
                push_wrapped(
                    &mut lines,
                    blank_gutter(),
                    &format!("! {source}: {message}"),
                    theme.transcript.error,
                    theme,
                    width,
                );
            }
        }

        if first_line < lines.len() && is_meaningful_ledger_item(item) {
            let stamp = timestamp_gutter(entry.timing.as_ref().map(|tm| tm.absolute.as_str()));
            stamp_first_line(&mut lines[first_line], &stamp, theme);
            if let Some(timing) = &entry.timing {
                append_timing(&mut lines[first_line], timing, theme, width);
            }
            push_hairline(&mut lines, theme, content_cols);
        } else if let Some(timing) = &entry.timing {
            if let Some(line) = lines.get_mut(first_line) {
                append_timing(line, timing, theme, width);
            }
        }
    }

    if let Some(footer) = super::turn_footer(entries) {
        push_wrapped(
            &mut lines,
            blank_gutter(),
            &footer,
            theme.transcript.muted,
            theme,
            width,
        );
    }

    lines
}

fn is_meaningful_ledger_item(item: &TranscriptItem) -> bool {
    // Live control chrome (permission ask panel, turn separators, worked
    // banners) is not a ledger event: no timestamp stamp, no hairline.
    !matches!(
        item,
        TranscriptItem::Banner { .. }
            | TranscriptItem::TurnSeparator
            | TranscriptItem::WorkedDuration(_)
            | TranscriptItem::PermissionAsk { .. }
    )
}

fn stamp_first_line(line: &mut Line<'static>, stamp: &str, theme: &Theme) {
    if line
        .spans
        .first()
        .is_some_and(|span| display_width(span.content.as_ref()) == GUTTER_WIDTH)
    {
        line.spans[0] = Span::styled(stamp.to_owned(), theme.transcript.gutter);
        return;
    }
    line.spans
        .insert(0, Span::styled(stamp.to_owned(), theme.transcript.gutter));
}

fn push_hairline(lines: &mut Vec<Line<'static>>, theme: &Theme, content_cols: usize) {
    lines.push(Line::from(vec![
        Span::styled(blank_gutter().to_owned(), theme.transcript.gutter),
        Span::styled(hairline_content(content_cols), theme.transcript.gutter),
    ]));
}

fn render_assistant_prose(content: &str, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    // Leave one right-edge cell unused. Exact-width writes can put terminals
    // into auto-wrap state, which makes table rows look clipped or disturbed
    // until a resize forces a different layout.
    let prose_width = content_width(width).saturating_sub(1).max(1);
    markdown::render_agent_markdown(content, theme, prose_width as u16)
        .into_iter()
        .map(|mut line| {
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            spans.push(Span::styled(
                blank_gutter().to_owned(),
                theme.transcript.gutter,
            ));
            spans.append(&mut line.spans);
            Line::from(spans).style(line.style)
        })
        .collect()
}

fn append_timing(line: &mut Line<'static>, timing: &EventTiming, theme: &Theme, width: u16) {
    let label = format!(" · {}", super::timing_label(timing));
    let used = line
        .spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum::<usize>();
    if used + display_width(&label) <= usize::from(width) {
        line.spans.push(Span::styled(label, theme.transcript.muted));
    }
}

#[allow(dead_code)]
pub(super) fn bottom_aligned(lines: Vec<Line<'static>>, height: u16) -> Vec<Line<'static>> {
    bottom_aligned_with_offset(lines, height, 0)
}

pub(super) fn bottom_aligned_with_offset(
    lines: Vec<Line<'static>>,
    height: u16,
    scroll_offset: usize,
) -> Vec<Line<'static>> {
    let height = usize::from(height);
    if height == 0 || lines.len() <= height {
        return lines;
    }

    let bottom_start = lines.len() - height;
    let start = bottom_start.saturating_sub(scroll_offset);
    lines.into_iter().skip(start).take(height).collect()
}

fn tool_result_label(name: &str) -> String {
    match name {
        "read_file" | "git_status" | "git_diff" => "explore".to_owned(),
        "run_shell" => "bash".to_owned(),
        "edit_file" => "edit".to_owned(),
        "" => "Used tool".to_owned(),
        _ => format!("Used tool {name}"),
    }
}

struct DetailRender {
    style: ratatui::style::Style,
    gutter: &'static str,
}

#[allow(dead_code)]
fn push_bounded_detail(
    lines: &mut Vec<Line<'static>>,
    detail: &str,
    render: DetailRender,
    theme: &Theme,
    width: u16,
    limit: usize,
) {
    if detail.is_empty() || limit == 0 {
        return;
    }

    let mut rendered_count = 0;
    let mut omitted_count = 0;

    for raw_line in output_rows_without_trailing_blanks(detail) {
        let wrapped = wrap_text(raw_line, content_width(width));
        for segment in wrapped {
            if rendered_count < limit {
                push_wrapped_segment(lines, render.gutter, segment, render.style, theme);
                rendered_count += 1;
            } else {
                omitted_count += 1;
            }
        }
    }

    if omitted_count > 0 {
        push_wrapped(
            lines,
            render.gutter,
            &format!("... +{omitted_count} rendered lines (bounded output)"),
            theme.transcript.muted,
            theme,
            width,
        );
    }
}

fn reasoning_summary(fidelity: &str, content: &str) -> String {
    let gist = reasoning_gist(content);
    let label = if fidelity == "summary" {
        "thought summary"
    } else {
        "thought"
    };
    format!("✱ {label} for 0s — {gist} · ctrl+o expand")
}

fn reasoning_gist(content: &str) -> String {
    let first_sentence = content
        .split_terminator(['.', '!', '?'])
        .next()
        .unwrap_or(content)
        .trim();
    let source = if first_sentence.is_empty() {
        content.trim()
    } else {
        first_sentence
    };
    truncate_gist(source, 60)
}

fn truncate_gist(source: &str, max_chars: usize) -> String {
    let mut chars = source.chars();
    let mut out = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

fn push_wrapped(
    lines: &mut Vec<Line<'static>>,
    gutter: &'static str,
    text: &str,
    style: ratatui::style::Style,
    theme: &Theme,
    width: u16,
) {
    debug_assert_eq!(display_width(gutter), GUTTER_WIDTH);
    for segment in wrap_text(text, content_width(width)) {
        push_wrapped_segment(lines, gutter, segment, style, theme);
    }
}

fn push_wrapped_with_continuation(
    lines: &mut Vec<Line<'static>>,
    content_prefixes: (&'static str, &'static str),
    text: &str,
    style: ratatui::style::Style,
    theme: &Theme,
    width: u16,
) {
    let (first_prefix, next_prefix) = content_prefixes;
    let body_width = content_width(width)
        .saturating_sub(display_width(first_prefix).max(display_width(next_prefix)))
        .max(1);
    let mut first_segment = true;
    for raw_line in text.split('\n') {
        for segment in wrap_text(raw_line, body_width) {
            let prefix = if first_segment {
                first_prefix
            } else {
                next_prefix
            };
            first_segment = false;
            lines.push(Line::from(vec![
                Span::styled(blank_gutter().to_owned(), theme.transcript.gutter),
                Span::styled(prefix.to_owned(), theme.transcript.gutter),
                Span::styled(segment, style),
            ]));
        }
    }
}

fn push_wrapped_segment(
    lines: &mut Vec<Line<'static>>,
    gutter: &'static str,
    segment: String,
    style: ratatui::style::Style,
    theme: &Theme,
) {
    debug_assert_eq!(display_width(gutter), GUTTER_WIDTH);
    lines.push(Line::from(vec![
        Span::styled(gutter.to_owned(), theme.transcript.gutter),
        Span::styled(segment, style),
    ]));
}
