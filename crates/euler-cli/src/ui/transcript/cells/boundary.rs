use super::*;

pub(in crate::ui::transcript) fn render_interrupted(
    lines: &mut Vec<Line<'static>>,
    theme: &Theme,
    width: u16,
) {
    // The ■ lives in the spine anchor (§1).
    let text = "interrupted — tell euler what to do differently".to_owned();
    push_wrapped_with_prefix(
        lines,
        CellPrefixes {
            first: blank_gutter(),
            next: blank_gutter(),
        },
        &text,
        theme.transcript.warning,
        theme,
        width,
    );
}

pub(in crate::ui::transcript) struct ExtensionResultRender<'a> {
    pub(in crate::ui::transcript) reference: &'a str,
    pub(in crate::ui::transcript) ok: bool,
    pub(in crate::ui::transcript) output: &'a str,
    pub(in crate::ui::transcript) limit: usize,
}

/// Extension command output as a flat foldable artifact row: colored verb
/// header + pretty-printed payload as dim tail lines (§4 ledger vocabulary).
pub(in crate::ui::transcript) fn render_extension_result(
    lines: &mut Vec<Line<'static>>,
    render: ExtensionResultRender<'_>,
    theme: &Theme,
    width: u16,
) {
    let (glyph, title_style) = if render.ok {
        (glyphs::check(), theme.transcript.tool)
    } else {
        (glyphs::cross(), theme.transcript.tool_error)
    };
    let rows = artifact::artifact_output_rows(render.output, render.limit.max(1));
    let body = artifact::plain_artifact_rows(&rows.rows, theme.transcript.muted);
    artifact::push_artifact_cell(
        lines,
        artifact::ArtifactCellRender {
            title: &format!("extension {} {glyph}", render.reference),
            title_suffix: None,
            rows: &body,
            footer: "",
            style: title_style,
            width,
        },
        theme,
    );
}

pub(in crate::ui::transcript) fn render_worked_duration(
    lines: &mut Vec<Line<'static>>,
    duration: &str,
    theme: &Theme,
    width: u16,
) {
    let label = format!("Worked for {duration}");
    let text = format!(" {label} ");
    let text_width = display_width(&text);
    let width = usize::from(width).max(1);
    let line = if width <= text_width {
        label
    } else {
        let remaining = width - text_width;
        let left = remaining / 2;
        let right = remaining - left;
        format!("{}{}{}", "─".repeat(left), text, "─".repeat(right))
    };
    lines.push(Line::from(Span::styled(line, theme.transcript.muted)));
}

pub(in crate::ui::transcript) fn render_turn_recap(
    lines: &mut Vec<Line<'static>>,
    summary: &str,
    files: Option<&str>,
    theme: &Theme,
    width: u16,
) {
    push_wrapped_with_prefix(
        lines,
        CellPrefixes {
            first: blank_gutter(),
            next: blank_gutter(),
        },
        summary,
        theme.transcript.muted,
        theme,
        width,
    );
    if let Some(files) = files.filter(|f| !f.is_empty()) {
        push_wrapped_with_prefix(
            lines,
            CellPrefixes {
                first: blank_gutter(),
                next: blank_gutter(),
            },
            files,
            theme.transcript.gutter,
            theme,
            width,
        );
    }
}

pub(in crate::ui::transcript) struct ResumeBoundaryRender<'a> {
    pub(in crate::ui::transcript) label: &'a str,
    pub(in crate::ui::transcript) recovery_closure_appended: bool,
    pub(in crate::ui::transcript) warning_count: usize,
    pub(in crate::ui::transcript) events_replayed: usize,
}

pub(crate) fn resume_boundary_decision_text(
    label: &str,
    recovery_closure_appended: bool,
    warning_count: usize,
) -> String {
    // The ✓ lives in the spine anchor (§1).
    let mut decision = format!("resumed session {label}");
    if recovery_closure_appended {
        decision.push_str(" · recovery closure appended");
    }
    if warning_count > 0 {
        decision.push_str(&format!(" · {warning_count} warnings"));
    }
    decision
}

pub(in crate::ui::transcript) fn render_resume_boundary(
    lines: &mut Vec<Line<'static>>,
    boundary: ResumeBoundaryRender<'_>,
    theme: &Theme,
    width: u16,
) {
    let decision = resume_boundary_decision_text(
        boundary.label,
        boundary.recovery_closure_appended,
        boundary.warning_count,
    );
    // blank_gutter first-prefix so the ✓ spine anchor stamps onto this row;
    // the record text itself is dim (§3: gold is pending, not settled).
    push_wrapped_with_prefix(
        lines,
        CellPrefixes {
            first: blank_gutter(),
            next: blank_gutter(),
        },
        &decision,
        theme.transcript.muted,
        theme,
        width,
    );

    let core = format!(
        "{} events replayed · model context folded to stubs",
        boundary.events_replayed
    );
    let text = format!(" {core} ");
    let text_width = display_width(&text);
    let width = usize::from(width).max(1);
    let min_rule = 4usize;
    let divider = if width <= text_width + min_rule * 2 {
        format!("────{text}────")
    } else {
        let remaining = width - text_width;
        let left = remaining / 2;
        let right = remaining - left;
        format!("{}{}{}", "─".repeat(left), text, "─".repeat(right))
    };
    lines.push(Line::from(Span::styled(divider, theme.transcript.muted)));
}
