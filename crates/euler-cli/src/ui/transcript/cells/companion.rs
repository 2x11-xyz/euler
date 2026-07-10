use super::*;
use crate::ui::transcript::{CompanionRow, CompanionStatus};

pub(in crate::ui::transcript) struct CompanionRender<'a> {
    pub(in crate::ui::transcript) name: &'a str,
    pub(in crate::ui::transcript) task: &'a str,
    pub(in crate::ui::transcript) status: &'a CompanionStatus,
    pub(in crate::ui::transcript) rows: &'a [CompanionRow],
    pub(in crate::ui::transcript) expanded: bool,
}

/// Max nested report/finding rows shown while a companion is still running.
const COMPANION_RUNNING_VISIBLE_ROWS: usize = 2;

pub(in crate::ui::transcript) fn render_companion_block(
    lines: &mut Vec<Line<'static>>,
    companion: CompanionRender<'_>,
    theme: &Theme,
    width: u16,
) {
    // The ◆ lives in the spine anchor (§1); block rows keep the teal rail.
    let name = if companion.name.is_empty() {
        "companion"
    } else {
        companion.name
    };
    match companion.status {
        CompanionStatus::Running { elapsed } => {
            render_companion_running(
                lines,
                CompanionRunningRender {
                    name,
                    task: companion.task,
                    elapsed: elapsed.as_deref().unwrap_or("0s"),
                    rows: companion.rows,
                },
                theme,
                width,
            );
        }
        CompanionStatus::Done {
            ok,
            summary,
            elapsed,
        } => {
            render_companion_done(
                lines,
                CompanionDoneRender {
                    name,
                    task: companion.task,
                    ok: *ok,
                    summary,
                    elapsed: elapsed.as_deref().unwrap_or("0s"),
                    rows: companion.rows,
                    expanded: companion.expanded,
                },
                theme,
                width,
            );
        }
    }
}

struct CompanionRunningRender<'a> {
    name: &'a str,
    task: &'a str,
    elapsed: &'a str,
    rows: &'a [CompanionRow],
}

fn render_companion_running(
    lines: &mut Vec<Line<'static>>,
    running: CompanionRunningRender<'_>,
    theme: &Theme,
    width: u16,
) {
    // §1: the ◆ is the spine anchor; the header text starts at the content
    // column and only child rows carry the teal rail.
    let header = if running.task.is_empty() {
        format!("{} ⠧ · {}", running.name, running.elapsed)
    } else {
        format!(
            "{} ⠧ · {} · {}",
            running.name, running.task, running.elapsed
        )
    };
    push_wrapped_with_prefix(
        lines,
        CellPrefixes {
            first: blank_gutter(),
            next: blank_gutter(),
        },
        &header,
        theme.transcript.companion,
        theme,
        width,
    );
    push_companion_rail_line(
        lines,
        "own ledger · own permission scope",
        theme.transcript.muted,
        theme,
        width,
    );
    let skip = running
        .rows
        .len()
        .saturating_sub(COMPANION_RUNNING_VISIBLE_ROWS);
    if skip > 0 {
        push_companion_rail_line(
            lines,
            &format!("… {skip} earlier reports folded"),
            theme.transcript.muted,
            theme,
            width,
        );
    }
    for row in running.rows.iter().skip(skip) {
        push_companion_row(lines, row, theme, width);
    }
}

struct CompanionDoneRender<'a> {
    name: &'a str,
    task: &'a str,
    ok: bool,
    summary: &'a str,
    elapsed: &'a str,
    rows: &'a [CompanionRow],
    expanded: bool,
}

fn render_companion_done(
    lines: &mut Vec<Line<'static>>,
    done: CompanionDoneRender<'_>,
    theme: &Theme,
    width: u16,
) {
    let findings = done
        .rows
        .iter()
        .filter(|row| matches!(row, CompanionRow::Finding { .. }))
        .count();
    let state = if done.ok { "done" } else { "failed" };
    let findings_part = if findings > 0 {
        format!(" · {findings} findings")
    } else if !done.rows.is_empty() {
        format!(" · {} reports", done.rows.len())
    } else {
        String::new()
    };
    if done.expanded {
        let header = format!("{} · {state} {}{findings_part}", done.name, done.elapsed);
        push_wrapped_with_prefix(
            lines,
            CellPrefixes {
                first: blank_gutter(),
                next: blank_gutter(),
            },
            &header,
            theme.transcript.companion,
            theme,
            width,
        );
        if !done.task.is_empty() {
            push_companion_rail_line(
                lines,
                &format!("task · {}", done.task),
                theme.transcript.muted,
                theme,
                width,
            );
        }
        for row in done.rows {
            push_companion_row(lines, row, theme, width);
        }
        if !done.summary.is_empty() {
            push_companion_rail_line(
                lines,
                &format!("summary · {}", done.summary),
                theme.transcript.muted,
                theme,
                width,
            );
        }
        push_companion_rail_line(
            lines,
            "ctrl+o collapse",
            theme.transcript.muted,
            theme,
            width,
        );
    } else {
        let summary_part = if done.summary.is_empty() {
            String::new()
        } else {
            format!(" · {}", done.summary)
        };
        let line = format!(
            "{} · {state} {}{findings_part}{summary_part} · ctrl+o expand",
            done.name, done.elapsed
        );
        push_wrapped_with_prefix(
            lines,
            CellPrefixes {
                first: blank_gutter(),
                next: blank_gutter(),
            },
            &line,
            theme.transcript.companion,
            theme,
            width,
        );
    }
}

fn push_companion_row(
    lines: &mut Vec<Line<'static>>,
    row: &CompanionRow,
    theme: &Theme,
    width: u16,
) {
    match row {
        CompanionRow::Finding { label, detail } => {
            let text = if detail.is_empty() {
                format!("finding · {label}")
            } else {
                format!("finding · {label}: {detail}")
            };
            push_companion_rail_line(lines, &text, theme.transcript.warning, theme, width);
        }
        CompanionRow::Report { text } => {
            push_companion_rail_line(lines, text, theme.transcript.muted, theme, width);
        }
    }
}

fn push_companion_rail_line(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    style: ratatui::style::Style,
    theme: &Theme,
    width: u16,
) {
    let rail = crate::ui::glyphs::companion_rail_prefix();
    let content_cols = content_width(width)
        .saturating_sub(display_width(rail))
        .max(1);
    for (index, segment) in wrap_text(text, content_cols).into_iter().enumerate() {
        let prefix = if index == 0 { rail } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(blank_gutter().to_owned(), theme.transcript.gutter),
            Span::styled(prefix.to_owned(), theme.transcript.companion),
            Span::styled(segment, style),
        ]));
    }
}
