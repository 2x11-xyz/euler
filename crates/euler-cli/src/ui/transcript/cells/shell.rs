use crate::ui::text::{display_width, truncate_display};

const SHELL_LABEL_MAX_COLUMNS: usize = 80;

pub(crate) fn normalized_shell_command(command: &str) -> String {
    let stripped = strip_bash_lc(command.trim());
    let lines = stripped
        .lines()
        .map(sanitized_shell_label_line)
        .map(trim_shell_summary_line)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let Some(first) = lines.first() else {
        return String::new();
    };
    let suffix = if lines.len() > 1 {
        format!(" … (+{} lines)", lines.len() - 1)
    } else {
        String::new()
    };
    bounded_shell_label(first, &suffix)
}

fn bounded_shell_label(first: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        if display_width(first) > SHELL_LABEL_MAX_COLUMNS {
            let truncated = truncate_display(first, SHELL_LABEL_MAX_COLUMNS.saturating_sub(1));
            format!("{truncated}…")
        } else {
            first.to_owned()
        }
    } else {
        let suffix_width = display_width(suffix);
        if suffix_width >= SHELL_LABEL_MAX_COLUMNS {
            return truncate_display(suffix, SHELL_LABEL_MAX_COLUMNS);
        }
        let first_budget = SHELL_LABEL_MAX_COLUMNS - suffix_width;
        let first = if display_width(first) > first_budget {
            truncate_display(first, first_budget).trim_end().to_owned()
        } else {
            first.to_owned()
        };
        format!("{first}{suffix}")
    }
}

fn sanitized_shell_label_line(line: &str) -> String {
    let mut output = String::new();
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            strip_ansi_sequence(&mut chars);
        } else if ch == '\t' {
            output.push(' ');
        } else if !ch.is_control() {
            output.push(ch);
        }
    }
    output
}

fn strip_ansi_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        Some('[') => strip_csi_sequence(chars),
        Some(']' | 'P' | '^' | '_' | 'X') => strip_string_escape_sequence(chars),
        Some(ch) if (' '..='/').contains(&ch) => strip_escape_with_intermediates(chars),
        Some(_) | None => {}
    }
}

fn strip_csi_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for ch in chars.by_ref() {
        if ('@'..='~').contains(&ch) {
            break;
        }
    }
}

fn strip_string_escape_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let mut saw_escape = false;
    for ch in chars.by_ref() {
        if ch == '\u{7}' {
            break;
        }
        if saw_escape && ch == '\\' {
            break;
        }
        saw_escape = ch == '\u{1b}';
    }
}

fn strip_escape_with_intermediates(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for ch in chars.by_ref() {
        if !(' '..='/').contains(&ch) {
            break;
        }
    }
}

fn trim_shell_summary_line(line: String) -> String {
    let trimmed = line.trim();
    trimmed
        .strip_suffix('\\')
        .map_or(trimmed, str::trim_end)
        .to_owned()
}

fn strip_bash_lc(command: &str) -> String {
    let Some(rest) = command.strip_prefix("bash -lc ") else {
        return command.to_owned();
    };
    unquote_shell_arg(rest.trim())
}

fn unquote_shell_arg(value: &str) -> String {
    if value.len() < 2 {
        return value.to_owned();
    }
    let Some(quote) = value
        .chars()
        .next()
        .filter(|quote| *quote == '"' || *quote == '\'')
    else {
        return value.to_owned();
    };
    if !value.ends_with(quote) {
        return value.to_owned();
    }
    let inner = &value[quote.len_utf8()..value.len() - quote.len_utf8()];
    if quote == '"' {
        unescape_double_quoted_shell(inner)
    } else {
        inner.to_owned()
    }
}

fn unescape_double_quoted_shell(inner: &str) -> String {
    let mut output = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                output.push(next);
            }
        } else {
            output.push(ch);
        }
    }
    output
}
