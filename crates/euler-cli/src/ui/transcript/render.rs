use super::cells::{
    edit_failure_status, output_rows_without_trailing_blanks, push_bounded_children,
    push_bounded_failure_children, push_cell_parent, push_child_rows, render_companion_block,
    render_edit_cell, render_file_change_cell, render_interrupted, render_patch_cell,
    render_permission_ask, render_permission_decision, render_resume_boundary, render_tool_run,
    render_turn_recap, render_worked_duration, tool_failure_status, CompanionRender, EditRender,
    FileChangeRender, PatchRender, PermissionAskView, PermissionDecisionView, ResumeBoundaryRender,
    ToolRunRender,
};
use super::file_diff::{render_file_diff_cell, FileDiffRender};
use super::{EventTiming, ProjectedEntry, TranscriptItem, TOOL_CALL_MAX_LINES};
use crate::ui::glyphs::{self, user_line_prefix};
use crate::ui::markdown;
use crate::ui::text::{
    blank_gutter, content_width, display_width, gutter_width, hairline_content, is_ledger_gutter,
    timestamp_gutter, tree_gutter_pipe, wrap_text,
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
            patch_detail_lines: super::super::patch_diff::DIFF_PREVIEW_ROWS + 1,
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
        let item_expanded = expanded_artifact_keys.contains(&super::artifact_key_for_index(index));
        let item_limits = if item_expanded {
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
                let elapsed = reasoning_elapsed(entries, index);
                let label = match fidelity.as_str() {
                    "summary" => "thought summary",
                    _ => "thought",
                };
                if item_expanded {
                    push_wrapped(
                        &mut lines,
                        blank_gutter(),
                        &format!(
                            "{} {label} for {elapsed} · ctrl+o collapse",
                            glyphs::thinking()
                        ),
                        theme.transcript.reasoning,
                        theme,
                        width,
                    );
                    push_wrapped(
                        &mut lines,
                        tree_gutter_pipe(),
                        content,
                        theme.transcript.reasoning,
                        theme,
                        width,
                    );
                } else {
                    push_wrapped(
                        &mut lines,
                        blank_gutter(),
                        &reasoning_summary(label, content, &elapsed),
                        theme.transcript.reasoning,
                        theme,
                        width,
                    );
                }
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
                let rows =
                    exploration_detail_rows(&super::coalesced_exploration_summaries(summaries));
                let header = tool_group_header("explore", rows.len(), entry.timing.as_ref());
                push_cell_parent(&mut lines, &header, theme.transcript.tool, theme, width);
                push_child_rows(&mut lines, &rows, theme.transcript.muted, theme, width);
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
                companion_name,
            } => {
                render_permission_ask(
                    &mut lines,
                    PermissionAskView {
                        capability,
                        reason,
                        command: command.as_deref(),
                        scope_prefix: scope_prefix.as_deref(),
                        companion_name: companion_name.as_deref(),
                    },
                    theme,
                    width,
                );
            }
            TranscriptItem::PermissionDecision {
                capability,
                decision,
                allowed,
                grant_scope,
                instruction,
            } => {
                render_permission_decision(
                    &mut lines,
                    PermissionDecisionView {
                        capability,
                        decision,
                        allowed: *allowed,
                        grant_scope: grant_scope.as_deref(),
                        instruction: instruction.as_deref(),
                    },
                    theme,
                    width,
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
                        "{} reverted {path} → ckpt {checkpoint_event_id} · files restored, history intact",
                        glyphs::revert()
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
            TranscriptItem::TurnRecap { summary, files } => {
                render_turn_recap(&mut lines, summary, files.as_deref(), theme, width);
            }
            TranscriptItem::ResumeBoundary {
                label,
                recovery_closure_appended,
                warning_count,
                events_replayed,
            } => {
                render_resume_boundary(
                    &mut lines,
                    ResumeBoundaryRender {
                        label,
                        recovery_closure_appended: *recovery_closure_appended,
                        warning_count: *warning_count,
                        events_replayed: *events_replayed,
                    },
                    theme,
                    width,
                );
            }
            TranscriptItem::Companion {
                name,
                task,
                status,
                rows,
                ..
            } => {
                let expanded = item_expanded || item_limits.output_lines == usize::MAX;
                render_companion_block(
                    &mut lines,
                    CompanionRender {
                        name,
                        task,
                        status,
                        rows,
                        expanded,
                    },
                    theme,
                    width,
                );
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
                if !item_renders_inline_timing(item) {
                    append_timing(&mut lines[first_line], timing, theme, width);
                }
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
            | TranscriptItem::TurnRecap { .. }
            | TranscriptItem::PermissionAsk { .. }
    )
}

fn item_renders_inline_timing(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::Exploration { .. } | TranscriptItem::Companion { .. }
    )
}

fn tool_group_header(label: &str, steps: usize, timing: Option<&EventTiming>) -> String {
    let mut parts = vec![label.to_owned(), step_count_label(steps)];
    if let Some(elapsed) = timing.and_then(|timing| timing.since_previous.as_deref()) {
        parts.push(elapsed.to_owned());
    }
    parts.join(" · ")
}

fn step_count_label(steps: usize) -> String {
    if steps == 1 {
        "1 step".to_owned()
    } else {
        format!("{steps} steps")
    }
}

fn exploration_detail_rows(rows: &[String]) -> Vec<String> {
    let parsed = rows
        .iter()
        .map(|row| split_exploration_row(row))
        .collect::<Vec<_>>();
    let verb_width = parsed
        .iter()
        .map(|(verb, _)| verb.chars().count())
        .max()
        .unwrap_or(0);
    parsed
        .into_iter()
        .map(|(verb, detail)| aligned_exploration_row(&verb, &detail, verb_width))
        .collect()
}

fn split_exploration_row(row: &str) -> (String, String) {
    for (prefix, verb) in [
        ("Read ", "Read"),
        ("Git ", "Git"),
        ("List ", "List"),
        ("Search ", "Search"),
    ] {
        if let Some(detail) = row.strip_prefix(prefix) {
            return (verb.to_owned(), detail.to_owned());
        }
    }
    ("Tool".to_owned(), row.to_owned())
}

fn aligned_exploration_row(verb: &str, detail: &str, verb_width: usize) -> String {
    if detail.is_empty() {
        verb.to_owned()
    } else {
        format!("{verb:<width$} {detail}", width = verb_width)
    }
}

fn stamp_first_line(line: &mut Line<'static>, stamp: &str, theme: &Theme) {
    if stamp.is_empty() && gutter_width() == 0 {
        // Timestamp column hidden: leave content unprefixed.
        return;
    }
    if line.spans.first().is_some_and(|span| {
        let width = display_width(span.content.as_ref());
        width == gutter_width() || (gutter_width() == 0 && width == 0)
    }) {
        if stamp.is_empty() {
            return;
        }
        line.spans[0] = Span::styled(stamp.to_owned(), theme.transcript.gutter);
        return;
    }
    if stamp.is_empty() {
        return;
    }
    line.spans
        .insert(0, Span::styled(stamp.to_owned(), theme.transcript.gutter));
}

fn push_hairline(lines: &mut Vec<Line<'static>>, theme: &Theme, content_cols: usize) {
    let hairline = Span::styled(hairline_content(content_cols), theme.transcript.hairline);
    if blank_gutter().is_empty() {
        lines.push(Line::from(vec![hairline]));
    } else {
        lines.push(Line::from(vec![
            Span::styled(blank_gutter().to_owned(), theme.transcript.gutter),
            hairline,
        ]));
    }
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

fn reasoning_summary(label: &str, content: &str, elapsed: &str) -> String {
    format!(
        "{} {label} for {elapsed} — {} · ctrl+o expand",
        glyphs::thinking(),
        reasoning_gist(content)
    )
}

fn reasoning_elapsed(entries: &[ProjectedEntry], index: usize) -> String {
    let start = entries.get(..index).and_then(|prior| {
        prior.iter().rev().find_map(|entry| match &entry.item {
            TranscriptItem::ModelCall { .. } => entry.timing.as_ref(),
            _ => None,
        })
    });
    let end = entries
        .get(index)
        .and_then(|entry| entry.timing.as_ref())
        .or_else(|| {
            entries
                .get(index + 1..)?
                .iter()
                .take_while(|entry| !matches!(entry.item, TranscriptItem::ModelCall { .. }))
                .find_map(|entry| match &entry.item {
                    TranscriptItem::ModelResult(_) => entry.timing.as_ref(),
                    _ => None,
                })
        });
    if let (Some(start), Some(end)) = (start, end) {
        if let (Some(start), Some(end)) = (
            parse_clock_seconds(&start.absolute),
            parse_clock_seconds(&end.absolute),
        ) {
            return super::format_duration((end - start).rem_euclid(24 * 60 * 60));
        }
    }
    end.and_then(|timing| {
        timing
            .since_previous
            .as_deref()
            .or(timing.since_start.as_deref())
            .filter(|elapsed| !elapsed.is_empty())
            .map(ToOwned::to_owned)
    })
    .unwrap_or_else(|| "0s".to_owned())
}

fn parse_clock_seconds(clock: &str) -> Option<i64> {
    let mut parts = clock.split(':');
    let hours = parts.next()?.parse::<i64>().ok()?;
    let minutes = parts.next()?.parse::<i64>().ok()?;
    let seconds = parts.next()?.parse::<i64>().ok()?;
    (parts.next().is_none() && hours < 24 && minutes < 60 && seconds < 60)
        .then_some(hours * 3600 + minutes * 60 + seconds)
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
    debug_assert!(
        is_ledger_gutter(gutter),
        "invalid ledger gutter: {gutter:?}"
    );
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
    debug_assert!(
        is_ledger_gutter(gutter),
        "invalid ledger gutter: {gutter:?}"
    );
    if gutter.is_empty() {
        lines.push(Line::from(vec![Span::styled(segment, style)]));
    } else {
        lines.push(Line::from(vec![
            Span::styled(gutter.to_owned(), theme.transcript.gutter),
            Span::styled(segment, style),
        ]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exploration_group_header_carries_steps_elapsed_and_tree_children() {
        let entries = vec![ProjectedEntry {
            item: TranscriptItem::Exploration {
                summaries: vec!["Read Cargo.toml".to_owned(), "Git diff".to_owned()],
            },
            timing: Some(EventTiming {
                absolute: "12:00:06".to_owned(),
                since_previous: Some("6s".to_owned()),
                since_start: Some("6s".to_owned()),
            }),
        }];

        let lines = render_projected_entries(
            &entries,
            &Theme::default(),
            80,
            TranscriptRenderLimits::default(),
        );
        let text = plain_text(&lines);

        assert!(text.contains("explore · 2 steps · 6s"), "text: {text:?}");
        assert!(text.contains("├ Read Cargo.toml"), "text: {text:?}");
        assert!(text.contains("└ Git  diff"), "text: {text:?}");
        assert!(!text.contains("└ Read Cargo.toml"), "text: {text:?}");
        assert!(!text.contains("├ Git  diff"), "text: {text:?}");
    }

    #[test]
    fn successful_shell_output_promotes_informative_result_line() {
        let item = TranscriptItem::ToolRun {
            command: "cargo test".to_owned(),
            ok: true,
            error: String::new(),
            output: "line 1\nline 2\nline 3\nline 4\ntest result: ok. 12 passed; 0 failed\ntail 1\ntail 2\n".to_owned(),
            exit_code: Some(0),
        };

        let lines = render_projected_items(
            &[item],
            &Theme::default(),
            96,
            TranscriptRenderLimits::default().with_output_lines(4),
        );
        let text = plain_text(&lines);

        assert!(
            text.contains("test result: ok. 12 passed; 0 failed")
                && text.contains("tail 2")
                && !text.contains("line 1"),
            "text: {text:?}"
        );
    }

    fn plain_text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
