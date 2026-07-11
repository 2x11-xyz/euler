use super::cells::{
    edit_failure_status, output_rows_without_trailing_blanks, push_bounded_children,
    push_bounded_failure_children, push_cell_parent, push_child_rows, render_companion_block,
    render_edit_cell, render_extension_result, render_file_change_cell, render_interrupted,
    render_patch_cell, render_permission_ask, render_permission_decision, render_resume_boundary,
    render_tool_run, render_turn_recap, render_worked_duration, tool_failure_status,
    CompanionRender, EditRender, ExtensionResultRender, FileChangeRender, PatchRender,
    PermissionAskView, PermissionDecisionView, ResumeBoundaryRender, ToolRunRender,
};
use super::file_diff::{render_file_diff_cell, FileDiffRender};
use super::{EventTiming, ProjectedEntry, TranscriptItem, TOOL_CALL_MAX_LINES};
use crate::ui::glyphs::{self, user_line_prefix};
use crate::ui::markdown;
use crate::ui::text::{
    blank_gutter, content_width, display_width, gutter_width, is_ledger_gutter, timestamp_gutter,
    timestamp_gutter_shown, tree_gutter_pipe, wrap_text,
};
use crate::ui::theme::Theme;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

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
    render_projected_items_with_expansion(items, theme, width, limits, false)
}

pub(super) fn render_projected_items_with_expansion(
    items: &[TranscriptItem],
    theme: &Theme,
    width: u16,
    limits: TranscriptRenderLimits,
    expanded: bool,
) -> Vec<Line<'static>> {
    let entries: Vec<_> = items
        .iter()
        .cloned()
        .map(|item| ProjectedEntry { item, timing: None })
        .collect();
    render_projected_entries_with_expansion(&entries, theme, width, limits, expanded)
}

#[cfg(test)]
pub(super) fn render_projected_entries(
    entries: &[ProjectedEntry],
    theme: &Theme,
    width: u16,
    limits: TranscriptRenderLimits,
) -> Vec<Line<'static>> {
    render_projected_entries_with_expansion(entries, theme, width, limits, false)
}

pub(super) fn render_projected_entries_with_expansion(
    entries: &[ProjectedEntry],
    theme: &Theme,
    width: u16,
    limits: TranscriptRenderLimits,
    expanded: bool,
) -> Vec<Line<'static>> {
    render_projected_entries_with_expansion_and_offsets(
        entries, theme, width, limits, expanded, true,
    )
    .0
}

/// Like `render_projected_entries_with_expansion`, additionally returning the
/// cumulative end-row offset of each entry. Offsets let the terminal commit
/// native scrollback at item boundaries so a width change can remap its
/// committed prefix exactly (no lost rows, no duplicates).
///
/// `show_turn_footer` renders the trailing "elapsed since first event"
/// footer when the last entry carries timing. That reads correctly for a
/// single bounded batch (the CLI/test transcript widget); the visual
/// canvas's incrementally growing whole-session history is never one batch,
/// so it passes `false`.
///
/// `expanded` is the single global `ctrl+o` fold state (issue #49) — every
/// foldable item in `entries` shares it; there is no per-item targeting.
#[allow(clippy::too_many_lines)] // ratchet: ledger projection match, refactor target
pub(super) fn render_projected_entries_with_expansion_and_offsets(
    entries: &[ProjectedEntry],
    theme: &Theme,
    width: u16,
    limits: TranscriptRenderLimits,
    expanded: bool,
    show_turn_footer: bool,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut lines = Vec::new();
    let mut item_end_offsets = Vec::with_capacity(entries.len());

    for (index, entry) in entries.iter().enumerate() {
        let first_line = lines.len();
        let item = &entry.item;
        let item_expanded = expanded;
        let item_limits = if item_expanded {
            limits.expanded()
        } else {
            limits
        };
        match item {
            TranscriptItem::Banner { .. } => {
                lines.extend(super::super::banner::styled_lines(theme));
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
                        &format!("{label} for {elapsed} · ctrl+o collapse"),
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
            TranscriptItem::ModelReasoningLive { elapsed } => {
                push_wrapped(
                    &mut lines,
                    blank_gutter(),
                    &format!("thinking · {elapsed}"),
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
                grant_source,
            } => {
                render_tool_run(
                    &mut lines,
                    ToolRunRender {
                        command,
                        ok: *ok,
                        error,
                        output,
                        exit_code: *exit_code,
                        grant_source: grant_source.as_deref(),
                    },
                    theme,
                    width,
                    item_limits.output_lines,
                );
            }
            TranscriptItem::Exploration { summaries } => {
                let rows = super::coalesced_exploration_summaries(summaries);
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
                user_rule_prefix,
                prior_count,
                selected_option,
                companion_name,
            } => {
                render_permission_ask(
                    &mut lines,
                    PermissionAskView {
                        capability,
                        reason,
                        command: command.as_deref(),
                        scope_prefix: scope_prefix.as_deref(),
                        user_rule_prefix: user_rule_prefix.as_deref(),
                        prior_count: *prior_count,
                        selected_option: *selected_option,
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
                        "reverted {path} → ckpt {checkpoint_event_id} · files restored, history intact"
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
            TranscriptItem::ExtensionResult {
                reference,
                ok,
                output,
            } => {
                render_extension_result(
                    &mut lines,
                    ExtensionResultRender {
                        reference,
                        ok: *ok,
                        output,
                        limit: item_limits.output_lines,
                    },
                    theme,
                    width,
                );
            }
            TranscriptItem::SessionSummary(summary) => {
                // No `* Summary:` plumbing label (#53): say the thing
                // plainly, muted, bullet-anchored like any other neutral
                // event — no stray asterisk.
                push_wrapped(
                    &mut lines,
                    blank_gutter(),
                    summary,
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
                    // The ✗ lives in the spine anchor (§1).
                    &format!("{source}: {message}"),
                    theme.transcript.error,
                    theme,
                    width,
                );
            }
            TranscriptItem::Notice(message) => {
                // No glyph, no source prefix — a plain muted line anchored
                // by the default `•` spine bullet (review v2 §14.4).
                push_wrapped(
                    &mut lines,
                    blank_gutter(),
                    message,
                    theme.transcript.muted,
                    theme,
                    width,
                );
            }
        }

        if first_line < lines.len() && is_meaningful_ledger_item(item) {
            let stamp = timestamp_gutter(entry.timing.as_ref().map(|tm| tm.absolute.as_str()));
            let anchor = spine_anchor(item, theme);
            stamp_first_line(&mut lines[first_line], &stamp, anchor.as_ref(), theme);
            if let Some(timing) = &entry.timing {
                if !item_renders_inline_timing(item) && timestamp_gutter_shown() {
                    append_timing(&mut lines[first_line], timing, theme, width);
                }
            }
        } else if let Some(timing) = &entry.timing {
            if timestamp_gutter_shown() {
                if let Some(line) = lines.get_mut(first_line) {
                    append_timing(line, timing, theme, width);
                }
            }
        }
        // §1: separation is the spine plus one blank line — applied after
        // every rendered event (dividers and recaps included) so batches
        // always end separated from whatever renders below. The renderer is
        // the single owner of vertical rhythm; no other layer adds spacers
        // around history content.
        // Banner lines end with their own built-in blank; everything else
        // gets the uniform one-blank separator here. Exception (review v2
        // §3/§6): a run of consecutive `Notice` items stacks directly — no
        // blank line between one notice and the next.
        let next_is_notice_continuation = matches!(item, TranscriptItem::Notice(_))
            && matches!(
                entries.get(index + 1).map(|entry| &entry.item),
                Some(TranscriptItem::Notice(_))
            );
        if first_line < lines.len()
            && !matches!(item, TranscriptItem::Banner { .. })
            && !next_is_notice_continuation
        {
            lines.push(Line::default());
        }
        item_end_offsets.push(lines.len());
    }

    if let Some(footer) = show_turn_footer
        .then(|| super::turn_footer(entries))
        .flatten()
    {
        push_wrapped(
            &mut lines,
            blank_gutter(),
            &footer,
            theme.transcript.muted,
            theme,
            width,
        );
        // The footer belongs to the last entry's committed region.
        if let Some(last) = item_end_offsets.last_mut() {
            *last = lines.len();
        }
    }

    (lines, item_end_offsets)
}

fn is_meaningful_ledger_item(item: &TranscriptItem) -> bool {
    super::item_wants_timestamp(item)
}

fn item_renders_inline_timing(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::Exploration { .. } | TranscriptItem::Companion { .. }
    )
}

fn tool_group_header(label: &str, steps: usize, timing: Option<&EventTiming>) -> String {
    let mut parts = vec![label.to_owned(), step_count_label(steps)];
    if timestamp_gutter_shown() {
        if let Some(elapsed) = timing.and_then(|timing| timing.since_previous.as_deref()) {
            parts.push(elapsed.to_owned());
        }
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

/// v2 anchor spine: glyph + style for an event's first row (§1). `None`
/// keeps the blank spine (separators have no anchor). Every anchor glyph —
/// including the user-message rail — sits flush in this same slot (review
/// v3 §R4); continuation rows for multi-line items that want the anchor
/// repeated (the user rail) place it themselves at the identical column.
fn spine_anchor(item: &TranscriptItem, theme: &Theme) -> Option<(String, Style)> {
    let anchor = match item {
        TranscriptItem::UserMessage(_) => (glyphs::user_rail().to_owned(), theme.transcript.gutter),
        TranscriptItem::Banner { .. }
        | TranscriptItem::TurnSeparator
        | TranscriptItem::WorkedDuration(_)
        | TranscriptItem::TurnRecap { .. } => return None,
        TranscriptItem::ModelReasoning { .. } | TranscriptItem::ModelReasoningLive { .. } => {
            (glyphs::thinking().to_owned(), theme.transcript.warning)
        }
        TranscriptItem::PermissionDecision { allowed, .. } => {
            if allowed.unwrap_or(false) {
                (glyphs::check().to_owned(), theme.transcript.added)
            } else {
                (glyphs::cross().to_owned(), theme.transcript.removed)
            }
        }
        TranscriptItem::ResumeBoundary { .. } => {
            (glyphs::check().to_owned(), theme.transcript.added)
        }
        TranscriptItem::Companion { .. } => (
            glyphs::companion_glyph().to_owned(),
            theme.transcript.companion,
        ),
        TranscriptItem::WorkspaceRestore { .. } => {
            (glyphs::revert().to_owned(), theme.transcript.added)
        }
        TranscriptItem::Interrupted => (glyphs::interrupt().to_owned(), theme.transcript.warning),
        TranscriptItem::Error { .. } => (glyphs::cross().to_owned(), theme.transcript.error),
        _ => (glyphs::bullet().to_owned(), theme.transcript.gutter),
    };
    Some(anchor)
}

/// Stamp an event's first row: `[HH:MM:SS ]` (when the gutter is opted in)
/// followed by the 2-cell spine anchor. Continuation rows keep the blank
/// prefix from `blank_gutter()`.
fn stamp_first_line(
    line: &mut Line<'static>,
    stamp: &str,
    anchor: Option<&(String, Style)>,
    theme: &Theme,
) {
    let prefix_width = gutter_width();
    let has_prefix = line
        .spans
        .first()
        .is_some_and(|span| display_width(span.content.as_ref()) == prefix_width);
    if !has_prefix {
        return;
    }
    let mut spans = Vec::with_capacity(2);
    if !stamp.is_empty() {
        spans.push(Span::styled(stamp.to_owned(), theme.transcript.gutter));
    }
    match anchor {
        Some((glyph, style)) => {
            let pad = crate::ui::text::SPINE_WIDTH.saturating_sub(display_width(glyph));
            spans.push(Span::styled(format!("{glyph}{}", " ".repeat(pad)), *style));
        }
        None => spans.push(Span::styled(
            crate::ui::text::BLANK_SPINE.to_owned(),
            theme.transcript.gutter,
        )),
    }
    line.spans.splice(0..1, spans);
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

#[cfg(test)]
pub(super) fn bottom_aligned(lines: Vec<Line<'static>>, height: u16) -> Vec<Line<'static>> {
    bottom_aligned_with_offset(lines, height, 0)
}

#[cfg(test)]
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
        let wrapped = wrap_text(raw_line, gutter_relative_width(width, render.gutter));
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
    // The ✱ lives in the spine anchor (§1).
    format!(
        "{label} for {elapsed} — {} · ctrl+o expand",
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
    // Wrap at the actual prefix width, not the generic 2-/11-cell gutter:
    // tree-nested content (`tree_gutter_pipe`/`_last`/`_mid` in narrow mode)
    // is wider than the plain spine, and reusing `content_width` here would
    // let every physical row run 2 cells past the terminal edge — the
    // overflow that resize exposed as a stale fragment at column 0 outside
    // the rail.
    let body_width = gutter_relative_width(width, gutter);
    for segment in wrap_text(text, body_width) {
        push_wrapped_segment(lines, gutter, segment, style, theme);
    }
}

/// Content width for a line prefixed by `gutter`, reserving exactly the
/// columns that prefix will occupy (rather than assuming the plain spine's
/// `gutter_width()`).
fn gutter_relative_width(width: u16, gutter: &str) -> usize {
    usize::from(width)
        .saturating_sub(display_width(gutter))
        .max(1)
}

/// Renders a multi-line block whose anchor (the user-message rail) repeats
/// on every physical row instead of just the first (review v3 §R4). The
/// rail lives in the same gutter-width slot every other anchor glyph uses:
/// the first row gets a `blank_gutter()` placeholder that the shared
/// spine-anchor stamp (`stamp_first_line`) swaps for the rail — flush at
/// column 0, exactly like `•`/`✓`/`✱`/etc — and continuation rows place the
/// rail themselves, right-aligned into that identical gutter-width slot (so
/// it lines up under the first row's rail even when the timestamp gutter is
/// on). Content starts immediately after, at the same column every anchor
/// uses.
fn push_wrapped_with_continuation(
    lines: &mut Vec<Line<'static>>,
    content_prefixes: (&'static str, &'static str),
    text: &str,
    style: ratatui::style::Style,
    theme: &Theme,
    width: u16,
) {
    let (_first_prefix, next_prefix) = content_prefixes;
    let body_width = content_width(width).max(1);
    let mut first_segment = true;
    for raw_line in text.split('\n') {
        for segment in wrap_text(raw_line, body_width) {
            let leading = if first_segment {
                blank_gutter().to_owned()
            } else {
                let pad = gutter_width().saturating_sub(display_width(next_prefix));
                format!("{}{next_prefix}", " ".repeat(pad))
            };
            first_segment = false;
            lines.push(Line::from(vec![
                Span::styled(leading, theme.transcript.gutter),
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
                summaries: vec![
                    "read Cargo.toml · 12 lines".to_owned(),
                    "git diff".to_owned(),
                ],
            },
            timing: Some(EventTiming {
                absolute: "12:00:06".to_owned(),
                since_previous: Some("6s".to_owned()),
                since_start: Some("6s".to_owned()),
            }),
        }];

        // Exploration step-elapsed is a timing decoration, toggle-gated in
        // the real app (review v2 §6); opt in here to exercise it directly.
        let lines = crate::ui::text::with_timestamp_gutter(true, || {
            render_projected_entries(
                &entries,
                &Theme::default(),
                80,
                TranscriptRenderLimits::default(),
            )
        });
        let text = plain_text(&lines);

        // Lowercase verbs, single space, per-step result data (design review
        // v3 §R3) — not the old capitalized, double-spaced alignment bug.
        assert!(text.contains("explore · 2 steps · 6s"), "text: {text:?}");
        assert!(
            text.contains("├ read Cargo.toml · 12 lines"),
            "text: {text:?}"
        );
        assert!(text.contains("└ git diff"), "text: {text:?}");
        assert!(!text.contains("└ read Cargo.toml"), "text: {text:?}");
        assert!(!text.contains("├ git diff"), "text: {text:?}");
        assert!(!text.contains("git  diff"), "text: {text:?}");
    }

    #[test]
    fn successful_shell_output_keeps_summary_tail_in_head_tail_preview() {
        // v4 amendment: the collapsed preview is the literal head + tail of
        // the buffer in buffer order — test summaries live in the tail, so
        // they stay visible without any informative-line promotion.
        let item = TranscriptItem::ToolRun {
            command: "cargo test".to_owned(),
            ok: true,
            error: String::new(),
            output: "line 1\nline 2\nline 3\nline 4\ntest result: ok. 12 passed; 0 failed\ntail 1\ntail 2\n".to_owned(),
            exit_code: Some(0),
            grant_source: None,
        };

        let lines = render_projected_items(
            &[item],
            &Theme::default(),
            96,
            TranscriptRenderLimits::default().with_output_lines(4),
        );
        let text = plain_text(&lines);

        assert!(
            text.contains("└ line 1")
                && text.contains("line 2")
                && text.contains("… 2 more lines · ctrl+o expand")
                && text.contains("test result: ok. 12 passed; 0 failed")
                && text.contains("tail 2")
                && !text.contains("line 3"),
            "text: {text:?}"
        );
    }

    #[test]
    fn expanded_thinking_body_rewraps_inside_the_rail_on_resize() {
        // Regression: the pipe-rail gutter (`tree_gutter_pipe`, 4 cells wide
        // in narrow/no-timestamp mode) was wrapped using the generic
        // 2-cell `content_width`, so every physical row ran 2 cells past
        // the terminal edge. On repaint at a narrower width the stale
        // overflow showed up as a fragment spilling to column 0, outside
        // the rail. Wrapping must key off the gutter actually rendered.
        let content = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu".to_owned();
        let item = TranscriptItem::ModelReasoning {
            fidelity: String::new(),
            content: content.clone(),
        };

        for width in [60_u16, 28_u16] {
            let lines = render_projected_items_with_expansion(
                std::slice::from_ref(&item),
                &Theme::default(),
                width,
                TranscriptRenderLimits::default(),
                true,
            );

            let pipe = tree_gutter_pipe();
            let mut body_words = Vec::new();
            for line in &lines {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                // No physical row — header or body — may run past the
                // rendered width: that overflow is exactly what spilled a
                // stale fragment to column 0 outside the rail on resize.
                assert!(
                    display_width(&text) <= usize::from(width),
                    "line {text:?} exceeds width {width} at rendered width"
                );
                // Body rows are identified by their gutter span (emitted by
                // `push_wrapped_segment`) being the pipe rail exactly; every
                // one of them must carry it — none may land bare at column 0.
                let is_body_row = line
                    .spans
                    .first()
                    .is_some_and(|s| s.content.as_ref() == pipe);
                if is_body_row {
                    body_words.push(text.trim_start_matches(pipe).trim().to_owned());
                }
            }
            assert!(
                body_words.len() > 1,
                "expected the body to wrap across multiple rail rows at width {width}"
            );
            let reassembled = body_words.join(" ");
            assert_eq!(
                reassembled.split_whitespace().collect::<Vec<_>>(),
                content.split_whitespace().collect::<Vec<_>>(),
                "rewrapped body at width {width} must reproduce the full content with no words lost"
            );
        }
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
