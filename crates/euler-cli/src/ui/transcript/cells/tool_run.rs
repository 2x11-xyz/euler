use super::*;
use crate::ui::text::truncate_display;

pub(in crate::ui::transcript) struct ToolRunRender<'a> {
    pub(in crate::ui::transcript) command: &'a str,
    pub(in crate::ui::transcript) ok: bool,
    pub(in crate::ui::transcript) error: &'a str,
    pub(in crate::ui::transcript) output: &'a str,
    pub(in crate::ui::transcript) exit_code: Option<i64>,
    /// "session" / "project" when covered by an existing grant.
    pub(in crate::ui::transcript) grant_source: Option<&'a str>,
    /// Auto-approved by static command-safety analysis.
    pub(in crate::ui::transcript) static_safe: bool,
}

pub(in crate::ui::transcript) fn render_tool_run(
    lines: &mut Vec<Line<'static>>,
    run: ToolRunRender<'_>,
    theme: &Theme,
    width: u16,
    limit: usize,
) {
    let style = if run.ok {
        theme.transcript.tool
    } else {
        theme.transcript.tool_error
    };
    // `limit == usize::MAX` is how an explicitly-expanded (ctrl+o) cell is
    // signaled by the renderer; anything else is a collapsed cell and gets
    // the head+tail preview (v4 spec amendment, docs/contracts/ui.md).
    // Expanded cells render the full stored buffer in buffer order — no
    // informative-line promotion, no reordering of any kind.
    let collapsed = limit != usize::MAX;
    let output = if collapsed {
        collapsed_tool_run_rows(run.output, limit)
    } else {
        artifact_output_rows(run.output, limit)
    };
    let rows = plain_artifact_rows(&output.rows, theme.transcript.muted);
    // The footer (status · line count · folded) is computed before the
    // heading so the heading truncation below can reserve room for it — the
    // metadata cluster must never itself get clipped by width-fitting
    // (design review v3 §R3).
    let footer = tool_run_footer(&run, output.total_rows, output.folded);
    let heading = tool_run_heading(&run, width, &footer);
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

const BASH_PREFIX: &str = "bash $ ";

/// Bash header, command text truncated (not the metadata) so the trailing
/// `· exit N · N lines · folded` cluster always renders intact at any width
/// (design review v3 §R3) — the old width-fit truncated the whole header,
/// sometimes clipping mid-metadata (`· 61 li`, `· exit` with no code). The
/// command yields all its width to the metadata cluster, down to nothing
/// (just an ellipsis) at extreme widths — a bare command reads worse than a
/// tight one, but a corrupted metadata cluster reads as a lost exit code.
fn tool_run_heading(run: &ToolRunRender<'_>, width: u16, footer: &str) -> String {
    if run.command.is_empty() {
        return "bash".to_owned();
    }
    // Durable user prefix rules read as `· user rule`, not "user grant".
    let grant_suffix = run.grant_source.map(|source| match source {
        "user" => " · user rule".to_owned(),
        _ => format!(" · {source} grant"),
    });
    let available = content_width(width);
    let reserved = display_width(BASH_PREFIX)
        + grant_suffix.as_deref().map(display_width).unwrap_or(0)
        + if run.static_safe {
            display_width(" · safe")
        } else {
            0
        }
        + display_width(" · ")
        + display_width(footer);
    let command_budget = available.saturating_sub(reserved);
    let command = if display_width(run.command) > command_budget {
        let truncated = truncate_display(run.command, command_budget.saturating_sub(1));
        format!("{truncated}…")
    } else {
        run.command.to_owned()
    };
    let mut heading = format!("{BASH_PREFIX}{command}");
    if let Some(suffix) = grant_suffix {
        // Provenance trace of an existing grant lives on the header (dim),
        // not as a standalone decision record (review v2 §8).
        heading.push_str(&suffix);
    }
    if run.static_safe {
        // Static-safety auto-approvals get the same quiet header trace: the
        // mode=static-safe decision record is suppressed in the transcript.
        heading.push_str(" · safe");
    }
    heading
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

/// Most informative output line worth surfacing from a completed command, if
/// any. Scored heuristic (design review v3 §R3, spec §0/§1): test summaries
/// outrank error/panic lines, which outrank count/total rows; ties keep the
/// earliest match. The collapsed tool-run preview no longer uses this (the
/// v4 head+tail amendment withdrew the one-selected-line rule); the surviving
/// consumer is the generic ToolResult failure preview
/// (`bounded_failure_preview_rows`), which still promotes the strongest
/// failure marker above the fold.
pub(in crate::ui::transcript) fn most_informative_line(output: &str) -> Option<&str> {
    let mut best: Option<(&str, u32)> = None;
    for line in output_rows_without_trailing_blanks(output) {
        let score = line_score(line);
        if score == 0 {
            continue;
        }
        let outranks_current = match best {
            Some((_, best_score)) => score > best_score,
            None => true,
        };
        if outranks_current {
            best = Some((line, score));
        }
    }
    best.map(|(line, _)| line)
}

fn is_informative_line(line: &str) -> bool {
    line_score(line) > 0
}

/// Priority tiers, highest first: test-run summaries, error/panic lines
/// (error weighted over warning), then count/total summary rows. Zero means
/// no signal at all.
fn line_score(line: &str) -> u32 {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return 0;
    }
    let lower = trimmed.to_ascii_lowercase();

    // Tier 1: test-run summaries — the most conclusive signal a collapsed
    // command can surface.
    if lower.contains("test result:") {
        return 400;
    }
    if contains_word_ci(trimmed, "failed") || has_count_token(&lower, &["passed", "failed"]) {
        return 380;
    }

    // Tier 2: error / panic lines outrank warnings. `contains_word_ci(_,
    // "error")` also covers the old bracketed-code form ("error[E0308]:
    // ..."), since `[` is a non-alphanumeric token boundary.
    if contains_word_ci(trimmed, "error")
        || contains_word_ci(trimmed, "panicked")
        || lower.contains("fatal")
    {
        return 300;
    }
    if contains_word_ci(trimmed, "warning") {
        return 250;
    }

    // Tier 3: counts / totals — grep/ripgrep/wc-style summary rows, e.g.
    // "42 matches" or a trailing "136152 total".
    if has_count_token(&lower, &["total", "totals"])
        || has_count_token(&lower, &["lines", "line"])
        || has_count_token(&lower, &["matches", "match"])
    {
        return 200;
    }

    0
}

/// True if `word` appears in `line` as a standalone, case-insensitive token —
/// i.e. bounded by non-alphanumeric characters (or the string ends). Plain
/// tool output regularly carries these markers in any case ("FAILED",
/// "Failed", "failed"), so a naive `line.to_ascii_lowercase().contains(word)`
/// would work for casing but also fires inside unrelated words that merely
/// contain the marker as a substring (e.g. "errorless", "warningless-mode").
/// Splitting on non-alphanumeric boundaries and comparing whole tokens
/// case-insensitively catches every casing of the real marker without that
/// over-matching.
fn contains_word_ci(line: &str, word: &str) -> bool {
    line.split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|token| token.eq_ignore_ascii_case(word))
}

/// True if some whitespace-delimited token in `line` is a bare integer
/// immediately followed by one of `words` (surrounding punctuation ignored),
/// e.g. "9 passed" or "136152 total".
fn has_count_token(line: &str, words: &[&str]) -> bool {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    tokens.windows(2).any(|pair| {
        let num = pair[0].trim_matches(|ch: char| !ch.is_ascii_digit());
        if num.is_empty() || !num.chars().all(|ch| ch.is_ascii_digit()) {
            return false;
        }
        let word = pair[1].trim_matches(|ch: char| !ch.is_ascii_alphabetic());
        words.contains(&word)
    })
}

fn tool_run_footer(run: &ToolRunRender<'_>, total_rows: usize, folded: bool) -> String {
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

/// Collapsed tool-run preview shape (v4 spec amendment, docs/contracts/
/// ui.md): the literal first `COLLAPSED_PREVIEW_HEAD_LINES` and literal last
/// `COLLAPSED_PREVIEW_TAIL_LINES` buffer rows, in buffer order, with the
/// fold marker between them (the Codex preview model). head 2 / tail 3
/// keeps the whole collapsed cell (header + 6 preview rows) inside the
/// default 10-row collapsed budget (`TOOL_CALL_MAX_LINES`) while giving the
/// tail — where test summaries and errors live — the larger share.
const COLLAPSED_PREVIEW_HEAD_LINES: usize = 2;
const COLLAPSED_PREVIEW_TAIL_LINES: usize = 3;

/// Collapsed-cell preview (v4 spec amendment; supersedes the "exactly one
/// `└ ` result line" rule of review v2 §14.2): head = the literal first N
/// buffer rows, tail = the literal last M rows, both in buffer order; the
/// `… K more lines · ctrl+o expand` fold marker sits between them and
/// carries the hidden count. The `└ ` elbow marks the first preview row,
/// with sibling rows indented two extra spaces to align under it. Outputs
/// short enough to fit (≤ limit rows, or ≤ head+tail rows) render whole
/// with no marker, so head and tail can never overlap.
fn collapsed_tool_run_rows(detail: &str, limit: usize) -> ArtifactOutputRows {
    let rows = normalized_output_rows(detail);
    let total_rows = rows.len();
    if total_rows == 0 {
        return ArtifactOutputRows {
            rows: vec![String::new()],
            total_rows,
            folded: false,
        };
    }
    let preview_rows = COLLAPSED_PREVIEW_HEAD_LINES + COLLAPSED_PREVIEW_TAIL_LINES;
    let folded = total_rows > limit && total_rows > preview_rows;
    let source = if folded {
        let hidden = total_rows - preview_rows;
        let mut source = rows[..COLLAPSED_PREVIEW_HEAD_LINES].to_vec();
        source.push(format!("… {hidden} more lines · ctrl+o expand"));
        source.extend(
            rows[total_rows - COLLAPSED_PREVIEW_TAIL_LINES..]
                .iter()
                .cloned(),
        );
        source
    } else {
        rows
    };

    let mut source = source.into_iter();
    let mut preview = Vec::with_capacity(total_rows.min(preview_rows + 1));
    if let Some(first) = source.next() {
        preview.push(format!("└ {first}"));
    }
    preview.extend(source.map(|row| format!("  {row}")));

    ArtifactOutputRows {
        rows: preview,
        total_rows,
        folded,
    }
}

/// Normalize raw `run_shell` output once, at transcript ingest
/// (`run_item_from_result`), upstream of BOTH the collapsed and the
/// expanded view:
///
/// - drop the literal leading `exit N` status row `euler-core::tools::
///   run_shell` emits ahead of bounded output — including its signed,
///   annotated timeout/kill form — since the cell header/footer already
///   own the exit status;
/// - strip trailing whitespace from every stored line, so render-time
///   width padding can never round-trip back into the stored buffer (the
///   resize re-emit path commits rendered, width-padded rows to native
///   scrollback; stored buffers must hold raw logical lines only).
///
/// Views render the stored buffer as-is. Normalizing here — instead of
/// per-view — is what keeps both views agreeing on line count and order
/// by construction.
pub(in crate::ui::transcript) fn normalize_tool_run_output(output: &str) -> String {
    let output = strip_leading_exit_code_row(output);
    let mut lines = output.lines().map(str::trim_end);
    let mut normalized = String::with_capacity(output.len());
    if let Some(first) = lines.next() {
        normalized.push_str(first);
    }
    for line in lines {
        normalized.push('\n');
        normalized.push_str(line);
    }
    normalized
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

/// Matches the "exit N" header `euler-core::tools::run_shell` emits ahead of
/// bounded output — including its signed, annotated timeout form: `exit -1
/// (command timed out after {timeout_ms} ms and was killed; pass timeout_ms
/// up to {MAX_SHELL_TIMEOUT_MS} for longer runs)`. Only accepting unsigned
/// digits (`parse::<u32>`-style) would leave that timeout/signal row
/// un-stripped, so it accepts an optional leading `-` and an optional
/// trailing parenthesized annotation, not just the bare unsigned form.
fn is_leading_exit_code_row(line: &str) -> bool {
    let Some(rest) = line.trim().strip_prefix("exit ") else {
        return false;
    };
    let rest = rest.strip_prefix('-').unwrap_or(rest);
    let digits_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits_len == 0 {
        return false;
    }
    let remainder = rest[digits_len..].trim_start();
    remainder.is_empty() || (remainder.starts_with('(') && remainder.ends_with(')'))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_leading_exit_status_row() {
        assert_eq!(
            normalize_tool_run_output("exit 0\nreal output"),
            "real output"
        );
        assert_eq!(normalize_tool_run_output("exit 12\na\nb"), "a\nb");
    }

    #[test]
    fn normalize_strips_signed_annotated_timeout_exit_row() {
        // Matches euler-core::tools::ShellExecutor::run_shell's real timeout
        // header verbatim (crates/euler-core/src/tools.rs).
        let output = "exit -1 (command timed out after 5000 ms and was killed; \
pass timeout_ms up to 600000 for longer runs)\nreal output";
        assert_eq!(normalize_tool_run_output(output), "real output");
    }

    #[test]
    fn normalize_keeps_non_status_first_lines() {
        assert_eq!(
            normalize_tool_run_output("exit code fine\nb"),
            "exit code fine\nb"
        );
        assert_eq!(normalize_tool_run_output("exit N\nb"), "exit N\nb");
        assert_eq!(normalize_tool_run_output("plain\nrows"), "plain\nrows");
    }

    #[test]
    fn normalize_strips_render_padding_from_stored_lines() {
        // Defensive: rendered rows are padded to the terminal width before
        // they hit native scrollback; if such a row ever round-trips toward
        // the stored buffer, ingest must drop the padding so buffers hold
        // raw logical lines only.
        let padded = format!("exit 0\n./.gitignore{}\n./CLAUDE.md", " ".repeat(300));
        assert_eq!(
            normalize_tool_run_output(&padded),
            "./.gitignore\n./CLAUDE.md"
        );
    }

    #[test]
    fn normalize_handles_exit_row_only_output() {
        assert_eq!(normalize_tool_run_output("exit 0"), "");
        assert_eq!(normalize_tool_run_output("exit 0\n"), "");
        assert_eq!(normalize_tool_run_output(""), "");
    }
}
