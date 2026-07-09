use std::cell::Cell;
use unicode_width::UnicodeWidthChar;

/// Fixed Warm Ledger timestamp column: `HH:MM:SS` + trailing space (9 cells).
pub(crate) const TIMESTAMP_GUTTER_WIDTH: usize = 9;

const BLANK_GUTTER: &str = "         "; // 9 spaces
                                        // 7 spaces + glyph + space = 9 (box-drawing glyphs are width 1)
const TREE_GUTTER_LAST: &str = "       └ ";
const TREE_GUTTER_MID: &str = "       ├ ";
const TREE_GUTTER_PIPE: &str = "       | ";
// Compact nesting when the timestamp column is hidden (content widens).
const TREE_GUTTER_LAST_NARROW: &str = "└ ";
const TREE_GUTTER_MID_NARROW: &str = "├ ";
const TREE_GUTTER_PIPE_NARROW: &str = "| ";

thread_local! {
    static SHOW_TIMESTAMP_GUTTER: Cell<bool> = const { Cell::new(true) };
}

/// Run `f` with the timestamp gutter column shown or hidden.
///
/// When hidden, ledger gutters collapse so the content column widens; hairlines
/// and nesting glyphs remain. Default (and outside this scope) is shown.
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
        TIMESTAMP_GUTTER_WIDTH
    } else {
        0
    }
}

pub(crate) fn content_width(width: u16) -> usize {
    usize::from(width).saturating_sub(gutter_width()).max(1)
}

pub(crate) fn blank_gutter() -> &'static str {
    if timestamp_gutter_shown() {
        BLANK_GUTTER
    } else {
        ""
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
        None => BLANK_GUTTER.to_owned(),
    }
}

pub(crate) fn hairline_content(content_cols: usize) -> String {
    "─".repeat(content_cols.max(1))
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
        width == TIMESTAMP_GUTTER_WIDTH
    } else {
        // Hidden timestamp column: blank (0) or compact tree prefixes.
        width == 0
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
    fn timestamp_gutter_is_nine_cells() {
        assert_eq!(display_width(&timestamp_gutter(Some("14:32:07"))), 9);
        assert_eq!(timestamp_gutter(Some("14:32:07")), "14:32:07 ");
        assert_eq!(display_width(blank_gutter()), 9);
        assert_eq!(display_width(tree_gutter_last()), 9);
        assert_eq!(display_width(tree_gutter_mid()), 9);
        assert_eq!(display_width(tree_gutter_pipe()), 9);
        assert_eq!(timestamp_gutter(None), blank_gutter());
    }

    #[test]
    fn hidden_timestamp_gutter_widens_content() {
        with_timestamp_gutter(false, || {
            assert_eq!(gutter_width(), 0);
            assert_eq!(blank_gutter(), "");
            assert_eq!(timestamp_gutter(Some("14:32:07")), "");
            assert_eq!(content_width(80), 80);
            assert_eq!(tree_gutter_last(), "└ ");
            assert!(is_ledger_gutter(""));
            assert!(is_ledger_gutter("└ "));
        });
        assert_eq!(gutter_width(), TIMESTAMP_GUTTER_WIDTH);
        assert_eq!(content_width(80), 71);
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
