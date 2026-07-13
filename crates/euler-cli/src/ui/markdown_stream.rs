//! Newline-gated markdown stream source collector.
//!
//! This module deliberately does not parse markdown. It only exposes stable
//! source boundaries so the UI can re-render complete lines without flickering
//! on half-received table rows, fences, or list markers.

use std::ops::Range;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct MarkdownStreamCollector {
    buffer: String,
    committed_len: usize,
}

impl MarkdownStreamCollector {
    pub(crate) fn push_delta(&mut self, delta: &str) {
        self.buffer.push_str(delta);
    }

    pub(crate) fn commit_complete_source(&mut self) -> Option<String> {
        let end = self.buffer.rfind('\n').map(|idx| idx + 1)?;
        let safe_end = safe_commit_end(&self.buffer, 0, end);
        if safe_end <= self.committed_len {
            return None;
        }
        let committed = self.buffer[self.committed_len..safe_end].to_owned();
        self.committed_len = safe_end;
        Some(committed)
    }

    pub(crate) fn visible_source(&self) -> Option<String> {
        let visible_end = self.visible_end();
        (visible_end > 0).then(|| self.buffer[..visible_end].to_owned())
    }

    pub(crate) fn committed_source(&self) -> Option<String> {
        (self.committed_len > 0).then(|| self.buffer[..self.committed_len].to_owned())
    }

    pub(crate) fn mutable_source(&self) -> Option<String> {
        let visible_end = self.visible_end();
        (visible_end > self.committed_len)
            .then(|| self.buffer[self.committed_len..visible_end].to_owned())
    }

    pub(crate) fn clear(&mut self) {
        self.buffer.clear();
        self.committed_len = 0;
    }

    fn visible_end(&self) -> usize {
        let Some(completed_end) = self.buffer.rfind('\n').map(|idx| idx + 1) else {
            return if is_safe_partial_preview("", &self.buffer) {
                self.buffer.len()
            } else {
                self.committed_len
            };
        };
        let visible_end = safe_visible_end(&self.buffer, 0, completed_end).max(self.committed_len);
        if visible_end < completed_end {
            return visible_end;
        }
        let tail = &self.buffer[visible_end..];
        if tail.is_empty() {
            return visible_end;
        }
        if is_safe_partial_preview(&self.buffer[..visible_end], tail) {
            self.buffer.len()
        } else {
            visible_end
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct FenceHold {
    marker: char,
    len: usize,
}

fn safe_commit_end(source: &str, start: usize, completed_end: usize) -> usize {
    safe_completed_end(source, start, completed_end, FencePolicy::HoldUntilClosed)
}

fn safe_visible_end(source: &str, start: usize, completed_end: usize) -> usize {
    safe_completed_end(
        source,
        start,
        completed_end,
        FencePolicy::ShowCompletedLines,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FencePolicy {
    HoldUntilClosed,
    ShowCompletedLines,
}

fn safe_completed_end(
    source: &str,
    start: usize,
    completed_end: usize,
    fence_policy: FencePolicy,
) -> usize {
    let mut safe_end = start;
    let mut pending_header: Option<Range<usize>> = None;
    let mut in_bare_table = false;
    let mut fence_hold: Option<FenceHold> = None;

    for range in complete_line_ranges(source, start, completed_end) {
        if let Some(fence) = fence_hold {
            if super::markdown::is_close_fence(&source[range.clone()], fence.marker, fence.len) {
                safe_end = range.end;
                fence_hold = None;
            } else if fence_policy == FencePolicy::ShowCompletedLines {
                safe_end = range.end;
            }
            continue;
        }

        if in_bare_table {
            if source[range.clone()].trim().is_empty() {
                safe_end = range.end;
                in_bare_table = false;
            } else if super::markdown::has_outer_table_pipe(&source[range.clone()]) {
                safe_end = range.end;
            } else {
                in_bare_table = false;
                safe_end = range.end;
            }
            continue;
        }

        if let Some(header) = pending_header.take() {
            if super::markdown::is_table_delimiter_line(&source[range.clone()]) {
                in_bare_table = true;
                safe_end = range.end;
                continue;
            }
            safe_end = header.end;
        }

        if let Some((marker, len, _markdown)) = super::markdown::open_fence(&source[range.clone()])
        {
            fence_hold = Some(FenceHold { marker, len });
            if fence_policy == FencePolicy::ShowCompletedLines {
                safe_end = range.end;
            }
        } else if source[range.clone()].trim().is_empty() {
            safe_end = range.end;
        } else if is_potential_bare_table_header(&source[range.clone()]) {
            pending_header = Some(range);
        } else {
            safe_end = range.end;
        }
    }

    safe_end
}

fn complete_line_ranges(source: &str, start: usize, completed_end: usize) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut cursor = start;
    while cursor < completed_end {
        let Some(newline_offset) = source[cursor..completed_end].find('\n') else {
            break;
        };
        let line_end = cursor + newline_offset + 1;
        ranges.push(cursor..line_end);
        cursor = line_end;
    }
    ranges
}

fn is_potential_bare_table_header(line: &str) -> bool {
    super::markdown::has_outer_table_pipe(line)
        && super::markdown::is_table_header_line(line)
        && !super::markdown::is_table_delimiter_line(line)
}

fn is_safe_partial_preview(prefix: &str, tail: &str) -> bool {
    if tail.is_empty() {
        return true;
    }
    if tail.contains('\n') {
        return false;
    }
    let trimmed = tail.trim();
    if trimmed.is_empty() {
        return true;
    }
    if is_potential_bare_table_header(tail) {
        return false;
    }
    if in_bare_table(prefix) && super::markdown::has_outer_table_pipe(tail) {
        return false;
    }
    !starts_markdown_table_fence(trimmed)
}

fn starts_markdown_table_fence(trimmed: &str) -> bool {
    let Some(rest) = trimmed
        .strip_prefix("```")
        .or_else(|| trimmed.strip_prefix("~~~"))
    else {
        return false;
    };
    rest.trim_start().starts_with("markdown")
}

fn in_bare_table(prefix: &str) -> bool {
    let mut saw_row = false;
    let mut saw_delimiter = false;
    for line in prefix.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return false;
        }
        if super::markdown::has_outer_table_pipe(line) {
            if super::markdown::is_table_delimiter_line(line) {
                saw_delimiter = true;
            } else if saw_delimiter {
                return true;
            } else {
                saw_row = true;
            }
            continue;
        }
        return false;
    }
    saw_row && saw_delimiter
}

#[cfg(test)]
mod tests {
    use super::MarkdownStreamCollector;

    #[test]
    fn buffers_until_newline() {
        let mut collector = MarkdownStreamCollector::default();
        collector.push_delta("hello");
        assert_eq!(collector.commit_complete_source(), None);
        assert_eq!(collector.visible_source(), Some("hello".to_owned()));

        collector.push_delta(" world\n");
        assert_eq!(
            collector.commit_complete_source(),
            Some("hello world\n".to_owned())
        );
        assert_eq!(collector.commit_complete_source(), None);
    }

    #[test]
    fn commits_only_newly_completed_prefix() {
        let mut collector = MarkdownStreamCollector::default();
        collector.push_delta("one\n");
        assert_eq!(collector.commit_complete_source(), Some("one\n".to_owned()));
        assert_eq!(collector.committed_source(), Some("one\n".to_owned()));
        assert_eq!(collector.mutable_source(), None);
        collector.push_delta("two");
        assert_eq!(collector.commit_complete_source(), None);
        assert_eq!(collector.visible_source(), Some("one\ntwo".to_owned()));
        assert_eq!(collector.mutable_source(), Some("two".to_owned()));
        collector.push_delta("\nthree");
        assert_eq!(collector.commit_complete_source(), Some("two\n".to_owned()));
        assert_eq!(collector.committed_source(), Some("one\ntwo\n".to_owned()));
        assert_eq!(collector.mutable_source(), Some("three".to_owned()));
        assert_eq!(
            collector.visible_source(),
            Some("one\ntwo\nthree".to_owned())
        );
    }

    #[test]
    fn commits_heading_prefix_only_once() {
        let mut collector = MarkdownStreamCollector::default();
        collector.push_delta("# Euler CLI Repo Summary\n");
        assert_eq!(
            collector.commit_complete_source(),
            Some("# Euler CLI Repo Summary\n".to_owned())
        );

        collector.push_delta("\nThe CLI owns the live/final handoff.\n");

        assert_eq!(
            collector.commit_complete_source(),
            Some("\nThe CLI owns the live/final handoff.\n".to_owned())
        );
    }

    #[test]
    fn bare_table_commits_each_complete_renderable_frame() {
        let mut collector = MarkdownStreamCollector::default();
        collector.push_delta("| A | B |\n");
        assert_eq!(collector.commit_complete_source(), None);
        assert_eq!(collector.visible_source(), None);
        collector.push_delta("|---|---|\n");
        assert_eq!(
            collector.commit_complete_source(),
            Some("| A | B |\n|---|---|\n".to_owned())
        );
        assert_eq!(
            collector.visible_source(),
            Some("| A | B |\n|---|---|\n".to_owned())
        );
        collector.push_delta("| 1 | 2 |\n");
        assert_eq!(
            collector.commit_complete_source(),
            Some("| 1 | 2 |\n".to_owned())
        );

        collector.push_delta("\n");
        assert_eq!(collector.commit_complete_source(), Some("\n".to_owned()));
        assert_eq!(
            collector.visible_source(),
            Some("| A | B |\n|---|---|\n| 1 | 2 |\n\n".to_owned())
        );
    }

    #[test]
    fn bare_table_after_prose_commits_without_retracting_prefix() {
        let mut collector = MarkdownStreamCollector::default();
        collector.push_delta("Intro before table.\n\n");
        assert_eq!(
            collector.commit_complete_source(),
            Some("Intro before table.\n\n".to_owned())
        );

        collector.push_delta("| A | B |\n");
        assert_eq!(collector.commit_complete_source(), None);
        collector.push_delta("|---|---|\n");
        assert_eq!(
            collector.commit_complete_source(),
            Some("| A | B |\n|---|---|\n".to_owned())
        );
        collector.push_delta("| 1 | 2 |\n");
        assert_eq!(
            collector.commit_complete_source(),
            Some("| 1 | 2 |\n".to_owned())
        );
        assert_eq!(
            collector.visible_source(),
            Some("Intro before table.\n\n| A | B |\n|---|---|\n| 1 | 2 |\n".to_owned())
        );
    }

    #[test]
    fn multiple_bare_tables_commit_incremental_frames() {
        let mut collector = MarkdownStreamCollector::default();
        collector.push_delta("| A | B |\n|---|---|\n| 1 | 2 |\n\n");
        assert_eq!(
            collector.commit_complete_source(),
            Some("| A | B |\n|---|---|\n| 1 | 2 |\n\n".to_owned())
        );

        collector.push_delta("Between.\n");
        assert_eq!(
            collector.commit_complete_source(),
            Some("Between.\n".to_owned())
        );
        collector.push_delta("| C | D |\n|---|---|\n");
        assert_eq!(
            collector.commit_complete_source(),
            Some("| C | D |\n|---|---|\n".to_owned())
        );
        collector.push_delta("| 3 | 4 |\n");
        assert_eq!(
            collector.commit_complete_source(),
            Some("| 3 | 4 |\n".to_owned())
        );
    }

    #[test]
    fn markdown_fenced_table_waits_for_closing_fence() {
        let mut collector = MarkdownStreamCollector::default();
        collector.push_delta("```markdown\n");
        assert_eq!(collector.commit_complete_source(), None);
        assert_eq!(collector.visible_source(), Some("```markdown\n".to_owned()));
        collector.push_delta("| A | B |\n|---|---|\n| 1 | 2 |\n");
        assert_eq!(collector.commit_complete_source(), None);
        assert_eq!(
            collector.visible_source(),
            Some("```markdown\n| A | B |\n|---|---|\n| 1 | 2 |\n".to_owned())
        );

        collector.push_delta("```\n");
        assert_eq!(
            collector.commit_complete_source(),
            Some("```markdown\n| A | B |\n|---|---|\n| 1 | 2 |\n```\n".to_owned())
        );
    }

    #[test]
    fn code_fence_waits_for_closing_fence() {
        let mut collector = MarkdownStreamCollector::default();
        collector.push_delta("```rust\nlet x = 1;\n");
        assert_eq!(collector.commit_complete_source(), None);
        assert_eq!(
            collector.visible_source(),
            Some("```rust\nlet x = 1;\n".to_owned())
        );
        assert_eq!(
            collector.mutable_source(),
            Some("```rust\nlet x = 1;\n".to_owned())
        );

        collector.push_delta("```\n");
        assert_eq!(
            collector.commit_complete_source(),
            Some("```rust\nlet x = 1;\n```\n".to_owned())
        );
        assert_eq!(
            collector.committed_source(),
            Some("```rust\nlet x = 1;\n```\n".to_owned())
        );
        assert_eq!(collector.mutable_source(), None);
    }

    #[test]
    fn partial_plain_prose_is_visible_before_newline() {
        let mut collector = MarkdownStreamCollector::default();
        collector.push_delta("The answer is still streaming");
        assert_eq!(
            collector.visible_source(),
            Some("The answer is still streaming".to_owned())
        );
        assert_eq!(collector.commit_complete_source(), None);
    }

    #[test]
    fn partial_table_body_waits_for_row_newline() {
        let mut collector = MarkdownStreamCollector::default();
        collector.push_delta("| A | B |\n|---|---|\n");
        assert_eq!(
            collector.commit_complete_source(),
            Some("| A | B |\n|---|---|\n".to_owned())
        );
        collector.push_delta("| 1 | 2 |");
        assert_eq!(
            collector.visible_source(),
            Some("| A | B |\n|---|---|\n".to_owned())
        );
    }

    #[test]
    fn non_table_pipe_line_commits_on_newline() {
        let mut collector = MarkdownStreamCollector::default();
        collector.push_delta("Escaped pipe in text: a | b | c\n");
        assert_eq!(
            collector.commit_complete_source(),
            Some("Escaped pipe in text: a | b | c\n".to_owned())
        );
    }
}
