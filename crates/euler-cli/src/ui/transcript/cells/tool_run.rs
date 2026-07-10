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
    let output = if run.ok {
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
