use std::cell::Cell;
use unicode_width::UnicodeWidthChar;

/// v2 anchor spine: a fixed 2-cell column at the left edge — event-type glyph
/// plus one space on an event's first row, two spaces on continuations.
pub(crate) const SPINE_WIDTH: usize = 2;
pub(crate) const BLANK_SPINE: &str = "  ";

/// Optional `/timestamps` gutter (§5.5): `HH:MM:SS` + space, rendered beside
/// the spine (whole column shifts right together).
pub(crate) const TIMESTAMP_GUTTER_WIDTH: usize = 9;

const BLANK_GUTTER: &str = "           "; // 9 (timestamps) + 2 (spine)
const TREE_GUTTER_LAST: &str = "         └ ";
const TREE_GUTTER_MID: &str = "         ├ ";
const TREE_GUTTER_PIPE: &str = "         | ";
// Spine-only nesting: children indent inside the parent at the content column.
const TREE_GUTTER_LAST_NARROW: &str = "  └ ";
const TREE_GUTTER_MID_NARROW: &str = "  ├ ";
const TREE_GUTTER_PIPE_NARROW: &str = "  | ";

thread_local! {
    // v2: timestamps are opt-in; the spine carries the ledger (§1).
    static SHOW_TIMESTAMP_GUTTER: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` with the timestamp gutter column shown or hidden.
///
/// When hidden (the default), only the 2-cell anchor spine prefixes content;
/// nesting glyphs indent at the content column. Opting in adds the 9-cell
/// timestamp column beside the spine.
pub(crate) fn with_timestamp_gutter<T>(show: bool, f: impl FnOnce() -> T) -> T {
    SHOW_TIMESTAMP_GUTTER.with(|cell| {
        let previous = cell.replace(show);
        let out = f();
        cell.set(previous);
        out
    })
}

pub(crate) fn timestamp_gutter_shown() -> bool {
    SHOW_TIMESTAMP_GUTTER.with(Cell::get)
}

pub(crate) fn gutter_width() -> usize {
    if timestamp_gutter_shown() {
        TIMESTAMP_GUTTER_WIDTH + SPINE_WIDTH
    } else {
        SPINE_WIDTH
    }
}

pub(crate) fn content_width(width: u16) -> usize {
    usize::from(width).saturating_sub(gutter_width()).max(1)
}

pub(crate) fn blank_gutter() -> &'static str {
    if timestamp_gutter_shown() {
        BLANK_GUTTER
    } else {
        BLANK_SPINE
    }
}

pub(crate) fn tree_gutter_last() -> &'static str {
    if timestamp_gutter_shown() {
        TREE_GUTTER_LAST
    } else {
        TREE_GUTTER_LAST_NARROW
    }
}

pub(crate) fn tree_gutter_mid() -> &'static str {
    if timestamp_gutter_shown() {
        TREE_GUTTER_MID
    } else {
        TREE_GUTTER_MID_NARROW
    }
}

pub(crate) fn tree_gutter_pipe() -> &'static str {
    if timestamp_gutter_shown() {
        TREE_GUTTER_PIPE
    } else {
        TREE_GUTTER_PIPE_NARROW
    }
}

pub(crate) fn timestamp_gutter(absolute: Option<&str>) -> String {
    if !timestamp_gutter_shown() {
        return String::new();
    }
    // The 2 spine cells are appended by the caller's anchor stamping; this
    // returns only the 9-cell timestamp column.
    match absolute {
        Some(ts) if display_width(ts) == 8 => {
            let mut out = String::with_capacity(TIMESTAMP_GUTTER_WIDTH);
            out.push_str(ts);
            out.push(' ');
            out
        }
        Some(ts) => {
            let mut out = truncate_display(ts, 8);
            while display_width(&out) < 8 {
                out.push(' ');
            }
            out.push(' ');
            if display_width(&out) > TIMESTAMP_GUTTER_WIDTH {
                truncate_display(&out, TIMESTAMP_GUTTER_WIDTH)
            } else {
                while display_width(&out) < TIMESTAMP_GUTTER_WIDTH {
                    out.push(' ');
                }
                out
            }
        }
        None => " ".repeat(TIMESTAMP_GUTTER_WIDTH),
    }
}

pub(crate) fn new_events_pill_text(new_events: usize) -> Option<String> {
    (new_events > 0).then(|| format!("↓ {new_events} new events"))
}

// F2 only adds the formatter; app.rs scroll-state wiring is the follow-up.
const _: fn(usize) -> Option<String> = new_events_pill_text;

/// True when `gutter` is a valid ledger prefix for the current gutter mode.
pub(crate) fn is_ledger_gutter(gutter: &str) -> bool {
    let width = display_width(gutter);
    if timestamp_gutter_shown() {
        // Timestamp column + spine cells, or a tree prefix at that width.
        width == TIMESTAMP_GUTTER_WIDTH + SPINE_WIDTH
    } else {
        // Spine-only mode: the 2-cell spine or compact tree prefixes.
        width == SPINE_WIDTH
            || gutter == TREE_GUTTER_LAST_NARROW
            || gutter == TREE_GUTTER_MID_NARROW
            || gutter == TREE_GUTTER_PIPE_NARROW
    }
}

pub(crate) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut wrapped = Vec::new();
    for raw_line in text.split('\n') {
        let mut current = String::new();
        let mut current_width = 0;
        for ch in raw_line.chars().filter(|ch| *ch != '\r') {
            let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if current_width + char_width > width && !current.is_empty() {
                if ch.is_whitespace() {
                    wrapped.push(current.trim_end().to_owned());
                    current.clear();
                    current_width = 0;
                    continue;
                } else if let Some(byte_idx) = current
                    .char_indices()
                    .rev()
                    .find_map(|(idx, item)| item.is_whitespace().then_some(idx))
                {
                    let remainder = current[byte_idx..].trim_start().to_owned();
                    current.truncate(byte_idx);
                    wrapped.push(current.trim_end().to_owned());
                    current = remainder;
                    current_width = display_width(&current);
                } else {
                    wrapped.push(current.trim_end().to_owned());
                    current.clear();
                    current_width = 0;
                }
            }
            current.push(ch);
            current_width += char_width;
        }
        wrapped.push(current.trim_end().to_owned());
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}

pub(crate) fn display_width(text: &str) -> usize {
    text.chars()
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum()
}

pub(crate) fn truncate_display(text: &str, max_width: usize) -> String {
    if display_width(text) <= max_width {
        return text.to_owned();
    }
    let mut output = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > max_width {
            break;
        }
        output.push(ch);
        width += char_width;
    }
    output
}

/// Truncate from the left, keeping the tail and prefixing an ellipsis —
/// footer §4 / issue #59: the directory truncates as `…/euler`, breaking at
/// a path-component boundary, never mid-component (`…11/euler`).
/// Char-boundary safe (multibyte glyphs never get split). Falls back to a
/// raw character cut only when a single component is itself wider than the
/// budget (there is no boundary to break at) — a deliberate last resort.
pub(crate) fn truncate_display_left(text: &str, max_width: usize) -> String {
    if display_width(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    const ELLIPSIS: char = '…';
    if max_width == 1 {
        return ELLIPSIS.to_string();
    }
    let budget = max_width - 1;
    let tail = component_boundary_suffix(text, budget).unwrap_or_else(|| raw_suffix(text, budget));
    format!("{ELLIPSIS}{tail}")
}

/// Widest suffix of `text` that starts right at a `/` boundary and fits
/// within `budget` display cells — the ellipsis then replaces only whole
/// leading path components. `None` when even the last component alone
/// (e.g. `/euler`) doesn't fit.
fn component_boundary_suffix(text: &str, budget: usize) -> Option<String> {
    let mut boundaries = text
        .char_indices()
        .filter(|(_, ch)| *ch == '/')
        .map(|(idx, _)| idx)
        .collect::<Vec<_>>();
    boundaries.sort_unstable();
    // Ascending byte offsets sort widest-suffix-first; the first fit is the
    // most of the path we can keep intact.
    boundaries
        .into_iter()
        .map(|idx| &text[idx..])
        .find(|suffix| display_width(suffix) <= budget)
        .map(str::to_owned)
}

/// Raw character-cut suffix fitting within `budget` cells, ignoring
/// component boundaries — the pre-#59 behavior, kept as the last-resort
/// fallback when no component boundary fits.
fn raw_suffix(text: &str, budget: usize) -> String {
    let mut collected: Vec<char> = Vec::new();
    let mut width = 0;
    for ch in text.chars().rev() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > budget {
            break;
        }
        collected.push(ch);
        width += char_width;
    }
    collected.reverse();
    collected.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_text_strips_carriage_returns_from_crlf_input() {
        assert_eq!(wrap_text("ab\r\ncd\r\n", 4), vec!["ab", "cd", ""]);
    }

    #[test]
    fn wrap_text_splits_lines_by_display_width() {
        assert_eq!(wrap_text("abcdef", 2), vec!["ab", "cd", "ef"]);
    }

    #[test]
    fn wrap_text_prefers_word_boundaries() {
        assert_eq!(
            wrap_text("alpha beta gamma delta", 12),
            vec!["alpha beta", "gamma delta"]
        );
    }

    #[test]
    fn truncate_display_respects_unicode_width_boundaries() {
        assert_eq!(truncate_display("ab\u{754c}cd", 4), "ab\u{754c}");
    }

    #[test]
    fn truncate_display_left_keeps_tail_and_prefixes_ellipsis() {
        assert_eq!(truncate_display_left("/2x11/euler", 7), "…/euler");
        assert_eq!(truncate_display_left("short", 20), "short");
        assert_eq!(truncate_display_left("short", 0), "");
        assert_eq!(truncate_display_left("short", 1), "…");
    }

    /// Issue #59: truncation must land on a path-component boundary — a
    /// budget that would previously bisect the "2x11" component (raw
    /// char-count-from-the-right) must instead sacrifice that whole
    /// component and land on the next "/" boundary.
    #[test]
    fn truncate_display_left_breaks_at_component_boundary_not_mid_component() {
        let text = "/Users/x/code/2x11/euler";
        // A budget of 9 (8 usable cells after the ellipsis) is exactly wide
        // enough for a raw last-8-chars cut to land inside "2x11" ("11" +
        // "/euler" = "11/euler"), the pre-fix bug. The fix instead drops the
        // whole "2x11" component and lands on the "/" before "euler".
        assert_eq!(truncate_display_left(text, 9), "…/euler");
        assert!(!truncate_display_left(text, 9).contains("11/euler"));

        // A wider budget that comfortably fits "/2x11/euler" keeps it whole.
        assert_eq!(truncate_display_left(text, 12), "…/2x11/euler");
    }

    /// When a single path component is itself wider than the budget, there
    /// is no component boundary to break at; falling back to a raw
    /// character cut (mid-component ellipsis) is an accepted last resort.
    #[test]
    fn truncate_display_left_falls_back_to_raw_cut_when_one_component_too_long() {
        let text = "/a/thisdirectorynameisverylongandwontfit";
        let truncated = truncate_display_left(text, 10);

        assert!(display_width(&truncated) <= 10);
        assert!(truncated.starts_with('…'));
        // No "/" boundary fits within the budget, so the cut necessarily
        // lands inside the long component rather than before it.
        assert!(!truncated.contains('/'));
    }

    #[test]
    fn truncate_display_left_is_multibyte_safe_at_the_boundary() {
        // Each CJK glyph is 2 display cells wide; the cut must land between
        // whole characters, never inside one.
        let text = "/repo/\u{754c}\u{754c}\u{754c}";
        let truncated = truncate_display_left(text, 5);
        assert!(truncated.chars().all(|ch| ch != '\u{fffd}'));
        assert!(display_width(&truncated) <= 5);
        assert!(truncated.starts_with('…'));
    }

    // v2 (§0/§1): timestamps are opt-in; the 2-cell anchor spine is the
    // default ledger prefix. `/timestamps` widens the column by 9 cells
    // beside the spine — it does not replace it.
    #[test]
    fn timestamp_gutter_opt_in_adds_nine_cells_beside_the_spine() {
        with_timestamp_gutter(true, || {
            assert_eq!(display_width(&timestamp_gutter(Some("14:32:07"))), 9);
            assert_eq!(timestamp_gutter(Some("14:32:07")), "14:32:07 ");
            assert_eq!(gutter_width(), TIMESTAMP_GUTTER_WIDTH + SPINE_WIDTH);
            assert_eq!(
                display_width(blank_gutter()),
                TIMESTAMP_GUTTER_WIDTH + SPINE_WIDTH
            );
            assert_eq!(
                display_width(tree_gutter_last()),
                TIMESTAMP_GUTTER_WIDTH + SPINE_WIDTH
            );
            assert_eq!(
                display_width(tree_gutter_mid()),
                TIMESTAMP_GUTTER_WIDTH + SPINE_WIDTH
            );
            assert_eq!(
                display_width(tree_gutter_pipe()),
                TIMESTAMP_GUTTER_WIDTH + SPINE_WIDTH
            );
            assert_eq!(timestamp_gutter(None), " ".repeat(TIMESTAMP_GUTTER_WIDTH));
        });
        // Restored to the default (hidden) gutter once the closure returns.
        assert_eq!(gutter_width(), SPINE_WIDTH);
    }

    #[test]
    fn default_gutter_is_the_two_cell_anchor_spine() {
        assert_eq!(gutter_width(), SPINE_WIDTH);
        assert_eq!(blank_gutter(), BLANK_SPINE);
        assert_eq!(timestamp_gutter(Some("14:32:07")), "");
        assert_eq!(content_width(80), 78);
        assert_eq!(tree_gutter_last(), "  └ ");
        assert_eq!(tree_gutter_mid(), "  ├ ");
        assert!(is_ledger_gutter(BLANK_SPINE));
        assert!(!is_ledger_gutter(""));
        assert!(is_ledger_gutter("  └ "));
        assert!(is_ledger_gutter("  ├ "));

        with_timestamp_gutter(true, || {
            assert_eq!(
                content_width(80),
                80 - (TIMESTAMP_GUTTER_WIDTH + SPINE_WIDTH)
            );
        });
        // Unaffected by a closure that has already returned.
        assert_eq!(content_width(80), 78);
    }

    #[test]
    fn new_events_pill_is_absent_when_no_events_arrived() {
        assert_eq!(new_events_pill_text(0), None);
    }

    #[test]
    fn new_events_pill_formats_arrived_event_count() {
        assert_eq!(new_events_pill_text(3), Some("↓ 3 new events".to_owned()));
    }
}
