use super::*;

pub(in crate::ui::transcript) struct ToolRunRender<'a> {
    pub(in crate::ui::transcript) command: &'a str,
    pub(in crate::ui::transcript) ok: bool,
    pub(in crate::ui::transcript) error: &'a str,
    pub(in crate::ui::transcript) output: &'a str,
    pub(in crate::ui::transcript) exit_code: Option<i64>,
    /// "session" / "project" when covered by an existing grant.
    pub(in crate::ui::transcript) grant_source: Option<&'a str>,
}

pub(in crate::ui::transcript) fn render_tool_run(
    lines: &mut Vec<Line<'static>>,
    run: ToolRunRender<'_>,
    theme: &Theme,
    width: u16,
    limit: usize,
) {
    let mut heading = if run.command.is_empty() {
        "bash".to_owned()
    } else {
        format!("bash $ {}", run.command)
    };
    if let Some(source) = run.grant_source {
        // Provenance trace of an existing grant lives on the header (dim),
        // not as a standalone decision record (review v2 §8).
        heading.push_str(&format!(" · {source} grant"));
    }
    let style = if run.ok {
        theme.transcript.tool
    } else {
        theme.transcript.tool_error
    };
    // `limit == usize::MAX` is how an explicitly-expanded (ctrl+o) cell is
    // signaled by the renderer; anything else is a collapsed cell (review v2
    // §14.2) and gets exactly one `└ ` result line instead of a raw output
    // preview that can read as if the exit status leaked into the body.
    let collapsed = limit != usize::MAX;
    let output = if collapsed {
        collapsed_tool_run_rows(run.output, run.ok, limit)
    } else if run.ok {
        tool_run_output_rows(run.output, limit)
    } else {
        informative_output_rows(run.output, limit)
    };
    let rows = plain_artifact_rows(&output.rows, theme.transcript.muted);
    let footer = tool_run_footer(run, output.total_rows, output.folded);
    push_artifact_cell(
        lines,
        ArtifactCellRender {
            title: &heading,
            title_suffix: None,
            rows: &rows,
            footer: &footer,
            style,
            width,
        },
        theme,
    );
}

pub(in crate::ui::transcript) fn tool_failure_status(
    exit_code: Option<i64>,
    error: &str,
) -> String {
    let cause = error.trim();
    let cross = glyphs::cross();
    match (exit_code, cause.is_empty()) {
        (Some(code), true) => format!("{cross} exit {code}"),
        (Some(code), false) => format!("{cross} exit {code}: {cause}"),
        (None, true) => format!("{cross} failed — no cause recorded"),
        (None, false) => format!("{cross} {cause}"),
    }
}

/// Edit/patch failure verb line: path + cause, never bare "failed".
pub(in crate::ui::transcript) fn edit_failure_status(path: &str, error: &str) -> String {
    let cause = error.trim();
    let cause = if cause.is_empty() {
        "no cause recorded"
    } else {
        cause
    };
    let path = path.trim();
    let cross = glyphs::cross();
    if path.is_empty() {
        format!("edit {cross} {cause}")
    } else {
        format!("edit {path} {cross} {cause}")
    }
}

/// First output line worth surfacing from a completed command, if any.
pub(in crate::ui::transcript) fn most_informative_line(output: &str) -> Option<&str> {
    output_rows_without_trailing_blanks(output)
        .into_iter()
        .find(|line| is_informative_line(line))
}

fn is_informative_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error[")
        || lower.contains("error:")
        || lower.contains("test result:")
        || lower.contains("failed")
        || lower.contains("panicked")
        || lower.contains("fatal")
}

fn tool_run_footer(run: ToolRunRender<'_>, total_rows: usize, folded: bool) -> String {
    let cross = glyphs::cross();
    let status = match (run.exit_code, run.ok) {
        (Some(code), true) => format!("exit {code}"),
        (Some(code), false) => format!("{cross} exit {code}"),
        (None, true) => "done".to_owned(),
        (None, false) if run.error.trim().is_empty() => {
            format!("{cross} failed — no cause recorded")
        }
        (None, false) => format!("{cross} {}", run.error.trim()),
    };
    let line_label = if total_rows == 1 {
        "1 line".to_owned()
    } else {
        format!("{total_rows} lines")
    };
    if folded {
        format!("{status} · {line_label} · folded")
    } else {
        format!("{status} · {line_label}")
    }
}

fn tool_run_output_rows(detail: &str, limit: usize) -> ArtifactOutputRows {
    if most_informative_line(detail).is_some() {
        informative_output_rows(detail, limit)
    } else {
        artifact_output_rows(detail, limit)
    }
}

/// Collapsed-cell preview: exactly one `└ ` result line carrying the most
/// informative output line (falling back to the first non-empty output
/// line), with any remaining preview rows indented two extra spaces
/// underneath it (review v2 §14.2, spec §0/§1).
fn collapsed_tool_run_rows(detail: &str, ok: bool, limit: usize) -> ArtifactOutputRows {
    let detail = strip_leading_exit_code_row(detail);
    let base = if ok {
        tool_run_output_rows(&detail, limit)
    } else {
        informative_output_rows(&detail, limit)
    };
    if base.total_rows == 0 {
        return base;
    }

    let mut rows = base.rows;
    let result_line = most_informative_line(&detail)
        .map(str::to_owned)
        .or_else(|| rows.iter().find(|row| !row.trim().is_empty()).cloned())
        .unwrap_or_default();
    if let Some(pos) = rows.iter().position(|row| *row == result_line) {
        rows.remove(pos);
    }

    let mut preview = Vec::with_capacity(rows.len() + 1);
    preview.push(format!("└ {result_line}"));
    preview.extend(rows.into_iter().map(|row| format!("  {row}")));

    ArtifactOutputRows {
        rows: preview,
        total_rows: base.total_rows,
        folded: base.folded,
    }
}

/// Shell output sometimes carries a literal leading "exit N" row ahead of
/// the real output; the header already owns the exit status, so drop that
/// row rather than let it masquerade as the first line of output.
fn strip_leading_exit_code_row(detail: &str) -> std::borrow::Cow<'_, str> {
    let mut lines = detail.splitn(2, '\n');
    let Some(first) = lines.next() else {
        return std::borrow::Cow::Borrowed(detail);
    };
    if is_leading_exit_code_row(first) {
        std::borrow::Cow::Borrowed(lines.next().unwrap_or(""))
    } else {
        std::borrow::Cow::Borrowed(detail)
    }
}

fn is_leading_exit_code_row(line: &str) -> bool {
    line.trim()
        .strip_prefix("exit ")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

fn informative_output_rows(detail: &str, limit: usize) -> ArtifactOutputRows {
    let rows = normalized_output_rows(detail);
    let total_rows = rows.len();
    if total_rows == 0 {
        return ArtifactOutputRows {
            rows: vec![String::new()],
            total_rows,
            folded: false,
        };
    }
    if total_rows <= limit {
        return ArtifactOutputRows {
            rows: promote_informative_row(rows),
            total_rows,
            folded: false,
        };
    }

    let informative = rows.iter().find(|row| is_informative_line(row)).cloned();
    let tail_n = OUTPUT_PREVIEW_TAIL_LINES.min(total_rows);
    let mut tail = rows[total_rows.saturating_sub(tail_n)..].to_vec();
    let mut preview = Vec::new();
    if let Some(line) = informative {
        // Keep the informative match as the first surfaced row even when it
        // already lives in the tail window.
        tail.retain(|row| row != &line);
        preview.push(line);
    }
    let hidden = total_rows.saturating_sub(preview.len() + tail.len());
    if hidden > 0 {
        preview.push(format!("… {hidden} more lines · ctrl+o expand"));
    }
    preview.extend(tail);
    ArtifactOutputRows {
        rows: preview,
        total_rows,
        folded: true,
    }
}

pub(super) fn promote_informative_row(mut rows: Vec<String>) -> Vec<String> {
    let Some(index) = rows.iter().position(|row| is_informative_line(row)) else {
        return rows;
    };
    if index == 0 {
        return rows;
    }
    let line = rows.remove(index);
    rows.insert(0, line);
    rows
}
