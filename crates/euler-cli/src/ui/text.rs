use unicode_width::UnicodeWidthChar;

// Indent/rail gutter for nested rows. Warm Ledger also wants a 9-char
// timestamp column on meaningful events (separate debt; do not overload this).
pub(crate) const GUTTER_WIDTH: usize = 4;

pub(crate) fn content_width(width: u16) -> usize {
    usize::from(width).saturating_sub(GUTTER_WIDTH).max(1)
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
}
