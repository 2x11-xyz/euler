use super::theme::Theme;
use ratatui::text::Span;
use std::path::Path;

pub(crate) const MAX_SYNTAX_DIFF_BYTES: usize = 48 * 1024;
pub(crate) const MAX_SYNTAX_DIFF_LINES: usize = 320;
pub(crate) const MAX_SYNTAX_LINE_BYTES: usize = 2 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SyntaxKind {
    Plain,
    Comment,
    Keyword,
    TypeName,
    Function,
    String,
    Number,
    Constant,
    Variable,
    Property,
    Operator,
    Punctuation,
    Macro,
    Attribute,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiffBodyKind {
    Context,
    Delete,
    Insert,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Language {
    CLike,
    Json,
    Python,
    Rust,
    Shell,
    Toml,
    TypeScriptLike,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Token {
    text: String,
    kind: SyntaxKind,
}

pub(crate) fn within_diff_budget(byte_len: usize, line_count: usize) -> bool {
    byte_len <= MAX_SYNTAX_DIFF_BYTES && line_count <= MAX_SYNTAX_DIFF_LINES
}

pub(crate) fn source_pair_within_budget(old: Option<&str>, new: Option<&str>) -> bool {
    let byte_len = old.unwrap_or_default().len() + new.unwrap_or_default().len();
    let line_count =
        source_line_count(old.unwrap_or_default()) + source_line_count(new.unwrap_or_default());
    within_diff_budget(byte_len, line_count)
}

fn source_line_count(source: &str) -> usize {
    if source.is_empty() {
        return 0;
    }
    source.bytes().filter(|byte| *byte == b'\n').count() + usize::from(!source.ends_with('\n'))
}

pub(crate) fn highlight_diff_body(
    path: &str,
    body: &str,
    kind: DiffBodyKind,
    theme: &Theme,
    enabled: bool,
) -> Vec<Span<'static>> {
    let fallback = plain_diff_body_span(body, kind, theme);
    if matches!(kind, DiffBodyKind::Delete) {
        return vec![fallback];
    }
    if !enabled || body.len() > MAX_SYNTAX_LINE_BYTES {
        return vec![fallback];
    }

    let Some(language) = detect_language(path) else {
        return vec![fallback];
    };
    let tokens = tokenize_line(body, language);
    if tokens.is_empty() {
        return vec![fallback];
    }

    tokens
        .into_iter()
        .map(|token| Span::styled(token.text, theme.scopes.syntax.style(token.kind)))
        .collect()
}

pub(crate) fn highlight_markdown_code_line(
    language_hint: &str,
    body: &str,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let fallback = Span::styled(body.to_owned(), theme.scopes.markup.code);
    if body.len() > MAX_SYNTAX_LINE_BYTES {
        return vec![fallback];
    }
    let Some(language) = detect_language_hint(language_hint) else {
        return vec![fallback];
    };
    let tokens = tokenize_line(body, language);
    if tokens.is_empty() {
        return vec![fallback];
    }
    tokens
        .into_iter()
        .map(|token| {
            Span::styled(
                token.text,
                theme
                    .scopes
                    .syntax
                    .style(token.kind)
                    .bg(theme.palette.surface),
            )
        })
        .collect()
}

pub(crate) fn enclosing_symbol(path: &str, source: &str, line_number: usize) -> Option<String> {
    if source.is_empty() || line_number == 0 {
        return None;
    }
    let language = detect_language(path)?;
    let lines = source.lines().take(line_number).collect::<Vec<_>>();
    lines
        .into_iter()
        .rev()
        .find_map(|line| symbol_from_line(line, language))
}

fn plain_diff_body_span(body: &str, kind: DiffBodyKind, theme: &Theme) -> Span<'static> {
    let style = match kind {
        DiffBodyKind::Context => theme.scopes.diff.context,
        DiffBodyKind::Delete => theme.scopes.diff.deleted_body,
        DiffBodyKind::Insert => theme.scopes.diff.inserted_body,
    };
    Span::styled(body.to_owned(), style)
}

fn detect_language(path: &str) -> Option<Language> {
    let path = Path::new(path);
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    match name.as_str() {
        "cargo.toml" | "pyproject.toml" => return Some(Language::Toml),
        "package.json" | "tsconfig.json" => return Some(Language::Json),
        "dockerfile" | "justfile" | "makefile" => return Some(Language::Shell),
        _ => {}
    }

    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "c" | "cc" | "cpp" | "cxx" | "h" | "hh" | "hpp" | "hxx" => Some(Language::CLike),
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => Some(Language::TypeScriptLike),
        "json" => Some(Language::Json),
        "py" | "pyw" => Some(Language::Python),
        "rs" => Some(Language::Rust),
        "sh" | "bash" | "zsh" | "fish" => Some(Language::Shell),
        "toml" => Some(Language::Toml),
        _ => None,
    }
}

fn detect_language_hint(hint: &str) -> Option<Language> {
    let normalized = hint
        .split(|ch: char| ch == ',' || ch.is_whitespace())
        .next()
        .unwrap_or_default()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    match normalized.as_str() {
        "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" => Some(Language::CLike),
        "js" | "jsx" | "javascript" | "mjs" | "ts" | "tsx" | "typescript" => {
            Some(Language::TypeScriptLike)
        }
        "json" => Some(Language::Json),
        "py" | "python" => Some(Language::Python),
        "rs" | "rust" => Some(Language::Rust),
        "bash" | "fish" | "sh" | "shell" | "zsh" => Some(Language::Shell),
        "toml" => Some(Language::Toml),
        _ => None,
    }
}

fn symbol_from_line(line: &str, language: Language) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with(['/', '#']) {
        return None;
    }
    match language {
        Language::Rust => rust_symbol(trimmed),
        Language::Python => python_symbol(trimmed),
        Language::TypeScriptLike => typescript_symbol(trimmed),
        Language::CLike => c_like_symbol(trimmed),
        Language::Shell => shell_symbol(trimmed),
        Language::Json | Language::Toml => None,
    }
}

fn rust_symbol(line: &str) -> Option<String> {
    for marker in ["fn ", "struct ", "enum ", "impl ", "trait ", "mod "] {
        if let Some(index) = line.find(marker) {
            let name = consume_symbol_name(&line[index + marker.len()..]);
            if !name.is_empty() {
                return Some(format_symbol(marker.trim(), name));
            }
        }
    }
    None
}

fn python_symbol(line: &str) -> Option<String> {
    for marker in ["def ", "class "] {
        if let Some(rest) = line.strip_prefix(marker) {
            let name = consume_symbol_name(rest);
            if !name.is_empty() {
                return Some(format_symbol(marker.trim(), name));
            }
        }
    }
    None
}

fn typescript_symbol(line: &str) -> Option<String> {
    for marker in ["function ", "class ", "interface ", "type "] {
        if let Some(index) = line.find(marker) {
            let name = consume_symbol_name(&line[index + marker.len()..]);
            if !name.is_empty() {
                return Some(format_symbol(marker.trim(), name));
            }
        }
    }
    line.find("=>").and_then(|arrow| {
        let left = line[..arrow].trim_end();
        let name = left
            .rsplit(|ch: char| !(ch == '_' || ch.is_alphanumeric()))
            .find(|part| !part.is_empty())?;
        Some(format!("{name}()"))
    })
}

fn c_like_symbol(line: &str) -> Option<String> {
    let paren = line.find('(')?;
    if line[..paren].contains('=') || line.ends_with(';') {
        return None;
    }
    let name = line[..paren]
        .rsplit(|ch: char| !(ch == '_' || ch.is_alphanumeric()))
        .find(|part| !part.is_empty())?;
    Some(format!("{name}()"))
}

fn shell_symbol(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("function ") {
        let name = consume_symbol_name(rest);
        if !name.is_empty() {
            return Some(format!("{name}()"));
        }
    }
    line.find("()")
        .map(|index| line[..index].trim())
        .filter(|name| !name.is_empty())
        .map(|name| format!("{name}()"))
}

fn consume_symbol_name(text: &str) -> &str {
    let text = text.trim_start_matches(|ch: char| ch == '<' || ch.is_whitespace());
    let end = text
        .find(|ch: char| !(ch == '_' || ch == '-' || ch.is_alphanumeric()))
        .unwrap_or(text.len());
    &text[..end]
}

fn format_symbol(kind: &str, name: &str) -> String {
    if kind == "fn" || kind == "def" || kind == "function" {
        format!("{name}()")
    } else {
        format!("{kind} {name}")
    }
}

fn tokenize_line(line: &str, language: Language) -> Vec<Token> {
    if line.is_empty() {
        return Vec::new();
    }
    if matches!(language, Language::Json) {
        return tokenize_json_line(line);
    }
    if matches!(language, Language::Toml) {
        return tokenize_toml_line(line);
    }

    let mut tokens = Vec::new();
    let mut index = 0;
    while index < line.len() {
        if push_whitespace(line, &mut index, &mut tokens) {
            continue;
        }
        if push_line_comment(line, index, language, &mut tokens) {
            break;
        }
        if push_block_comment(line, &mut index, language, &mut tokens) {
            continue;
        }
        if push_attribute(line, &mut index, language, &mut tokens) {
            continue;
        }
        if push_string(line, &mut index, language, &mut tokens) {
            continue;
        }
        if push_number(line, &mut index, &mut tokens) {
            continue;
        }
        if push_identifier(line, &mut index, language, &mut tokens) {
            continue;
        }
        if push_operator_or_punctuation(line, &mut index, &mut tokens) {
            continue;
        }
        push_plain_char(line, &mut index, &mut tokens);
    }
    tokens
}

fn push_whitespace(line: &str, index: &mut usize, tokens: &mut Vec<Token>) -> bool {
    let start = *index;
    let mut end = start;
    for (offset, ch) in line[start..].char_indices() {
        if !ch.is_whitespace() {
            break;
        }
        end = start + offset + ch.len_utf8();
    }
    if end == start {
        return false;
    }
    tokens.push(token(&line[start..end], SyntaxKind::Plain));
    *index = end;
    true
}

fn push_line_comment(
    line: &str,
    index: usize,
    language: Language,
    tokens: &mut Vec<Token>,
) -> bool {
    let rest = &line[index..];
    if matches!(language, Language::Python | Language::Shell) && rest.starts_with('#') {
        tokens.push(token(rest, SyntaxKind::Comment));
        return true;
    }
    if matches!(
        language,
        Language::CLike | Language::Rust | Language::TypeScriptLike
    ) && rest.starts_with("//")
    {
        tokens.push(token(rest, SyntaxKind::Comment));
        return true;
    }
    false
}

fn push_block_comment(
    line: &str,
    index: &mut usize,
    language: Language,
    tokens: &mut Vec<Token>,
) -> bool {
    if !matches!(
        language,
        Language::CLike | Language::Rust | Language::TypeScriptLike
    ) || !line[*index..].starts_with("/*")
    {
        return false;
    }
    let rest = &line[*index..];
    let len = rest.find("*/").map_or(rest.len(), |end| end + 2);
    tokens.push(token(&rest[..len], SyntaxKind::Comment));
    *index += len;
    true
}

fn push_attribute(
    line: &str,
    index: &mut usize,
    language: Language,
    tokens: &mut Vec<Token>,
) -> bool {
    let rest = &line[*index..];
    let at_line_start = line[..*index].trim().is_empty();
    match language {
        Language::Rust if at_line_start && (rest.starts_with("#[") || rest.starts_with("#![")) => {
            tokens.push(token(rest, SyntaxKind::Attribute));
            *index = line.len();
            true
        }
        Language::CLike if at_line_start && rest.starts_with('#') => {
            tokens.push(token(rest, SyntaxKind::Attribute));
            *index = line.len();
            true
        }
        Language::Python if at_line_start && rest.starts_with('@') => {
            let end = consume_until_whitespace(line, *index);
            tokens.push(token(&line[*index..end], SyntaxKind::Attribute));
            *index = end;
            true
        }
        _ => false,
    }
}

fn push_string(line: &str, index: &mut usize, language: Language, tokens: &mut Vec<Token>) -> bool {
    if matches!(language, Language::Rust) && push_rust_raw_string(line, index, tokens) {
        return true;
    }
    let Some(ch) = line[*index..].chars().next() else {
        return false;
    };
    let quote = match ch {
        '"' => ch,
        '`' if matches!(language, Language::TypeScriptLike) => ch,
        '\'' if !(matches!(language, Language::Rust) && looks_like_lifetime(line, *index)) => ch,
        _ => return false,
    };
    let end = consume_quoted(line, *index, quote);
    tokens.push(token(&line[*index..end], SyntaxKind::String));
    *index = end;
    true
}

fn push_rust_raw_string(line: &str, index: &mut usize, tokens: &mut Vec<Token>) -> bool {
    let rest = &line[*index..];
    let bytes = rest.as_bytes();
    if bytes.first().copied() != Some(b'r') {
        return false;
    }
    let mut hash_count = 0;
    while bytes.get(1 + hash_count).copied() == Some(b'#') {
        hash_count += 1;
    }
    if bytes.get(1 + hash_count).copied() != Some(b'"') {
        return false;
    }
    let close = format!("\"{}", "#".repeat(hash_count));
    let content_start = 2 + hash_count;
    let end = rest[content_start..]
        .find(&close)
        .map_or(rest.len(), |offset| content_start + offset + close.len());
    tokens.push(token(&rest[..end], SyntaxKind::String));
    *index += end;
    true
}

fn looks_like_lifetime(line: &str, index: usize) -> bool {
    let mut chars = line[index..].chars();
    if chars.next() != Some('\'') {
        return false;
    }
    let Some(next) = chars.next() else {
        return false;
    };
    (next.is_ascii_alphabetic() || next == '_') && !matches!(chars.next(), Some('\''))
}

fn consume_quoted(line: &str, start: usize, quote: char) -> usize {
    let mut escaped = false;
    let mut first = true;
    for (offset, ch) in line[start..].char_indices() {
        let end = start + offset + ch.len_utf8();
        if first {
            first = false;
            continue;
        }
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            return end;
        }
    }
    line.len()
}

fn push_number(line: &str, index: &mut usize, tokens: &mut Vec<Token>) -> bool {
    let Some(ch) = line[*index..].chars().next() else {
        return false;
    };
    if !ch.is_ascii_digit() {
        return false;
    }
    let start = *index;
    let end = consume_number(line, start);
    tokens.push(token(&line[start..end], SyntaxKind::Number));
    *index = end;
    true
}

fn consume_number(line: &str, start: usize) -> usize {
    let mut end = start;
    let mut previous = None;
    for (offset, ch) in line[start..].char_indices() {
        if ch.is_ascii_alphanumeric()
            || matches!(ch, '_' | '.')
            || (matches!(ch, '+' | '-') && matches!(previous, Some('e' | 'E')))
        {
            end = start + offset + ch.len_utf8();
        } else {
            break;
        }
        previous = Some(ch);
    }
    end
}

fn push_identifier(
    line: &str,
    index: &mut usize,
    language: Language,
    tokens: &mut Vec<Token>,
) -> bool {
    let Some(ch) = line[*index..].chars().next() else {
        return false;
    };
    if !is_identifier_start(ch) {
        return false;
    }
    let start = *index;
    let end = consume_identifier(line, start);
    let mut text_end = end;
    let kind = if matches!(language, Language::Rust) && rust_macro_bang_follows(line, end) {
        text_end += 1;
        SyntaxKind::Macro
    } else {
        classify_identifier(line, start, end, language)
    };
    tokens.push(token(&line[start..text_end], kind));
    *index = text_end;
    true
}

fn rust_macro_bang_follows(line: &str, index: usize) -> bool {
    line[index..].starts_with('!') && !line[index + 1..].starts_with('=')
}

fn consume_identifier(line: &str, start: usize) -> usize {
    let mut end = start;
    for (offset, ch) in line[start..].char_indices() {
        if offset == 0 {
            if !is_identifier_start(ch) {
                break;
            }
        } else if !is_identifier_continue(ch) {
            break;
        }
        end = start + offset + ch.len_utf8();
    }
    end
}

fn classify_identifier(line: &str, start: usize, end: usize, language: Language) -> SyntaxKind {
    let ident = &line[start..end];
    if is_keyword(language, ident) {
        return SyntaxKind::Keyword;
    }
    if is_constant(language, ident) {
        return SyntaxKind::Constant;
    }
    if is_type_name(language, ident) {
        return SyntaxKind::TypeName;
    }
    let next = next_non_space(line, end);
    if previous_non_space(line, start) == Some('.')
        || (matches!(language, Language::Toml) && next == Some('='))
    {
        return SyntaxKind::Property;
    }
    if next == Some('(') {
        return SyntaxKind::Function;
    }
    if ident.chars().next().is_some_and(char::is_uppercase) {
        return SyntaxKind::TypeName;
    }
    SyntaxKind::Variable
}

fn push_operator_or_punctuation(line: &str, index: &mut usize, tokens: &mut Vec<Token>) -> bool {
    let Some(ch) = line[*index..].chars().next() else {
        return false;
    };
    if is_operator(ch) {
        let start = *index;
        let mut end = start;
        for (offset, ch) in line[start..].char_indices() {
            if !is_operator(ch) {
                break;
            }
            end = start + offset + ch.len_utf8();
        }
        tokens.push(token(&line[start..end], SyntaxKind::Operator));
        *index = end;
        return true;
    }
    if is_punctuation(ch) {
        let end = *index + ch.len_utf8();
        tokens.push(token(&line[*index..end], SyntaxKind::Punctuation));
        *index = end;
        return true;
    }
    false
}

fn push_plain_char(line: &str, index: &mut usize, tokens: &mut Vec<Token>) {
    let Some(ch) = line[*index..].chars().next() else {
        return;
    };
    let end = *index + ch.len_utf8();
    tokens.push(token(&line[*index..end], SyntaxKind::Plain));
    *index = end;
}

fn tokenize_json_line(line: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < line.len() {
        if push_whitespace(line, &mut index, &mut tokens) {
            continue;
        }
        if line[index..].starts_with('"') {
            let end = consume_quoted(line, index, '"');
            let kind = if next_non_space(line, end) == Some(':') {
                SyntaxKind::Property
            } else {
                SyntaxKind::String
            };
            tokens.push(token(&line[index..end], kind));
            index = end;
            continue;
        }
        if push_number(line, &mut index, &mut tokens) {
            continue;
        }
        if push_json_constant(line, &mut index, &mut tokens) {
            continue;
        }
        if push_operator_or_punctuation(line, &mut index, &mut tokens) {
            continue;
        }
        push_plain_char(line, &mut index, &mut tokens);
    }
    tokens
}

fn push_json_constant(line: &str, index: &mut usize, tokens: &mut Vec<Token>) -> bool {
    for word in ["true", "false", "null"] {
        if line[*index..].starts_with(word) {
            let end = *index + word.len();
            tokens.push(token(&line[*index..end], SyntaxKind::Constant));
            *index = end;
            return true;
        }
    }
    false
}

fn tokenize_toml_line(line: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < line.len() {
        if push_whitespace(line, &mut index, &mut tokens) {
            continue;
        }
        if line[index..].starts_with('#') {
            tokens.push(token(&line[index..], SyntaxKind::Comment));
            break;
        }
        if matches!(line[index..].chars().next(), Some('"') | Some('\'')) {
            let quote = line[index..].chars().next().expect("quote");
            let end = consume_quoted(line, index, quote);
            tokens.push(token(&line[index..end], SyntaxKind::String));
            index = end;
            continue;
        }
        if push_number(line, &mut index, &mut tokens) {
            continue;
        }
        if push_identifier(line, &mut index, Language::Toml, &mut tokens) {
            continue;
        }
        if push_operator_or_punctuation(line, &mut index, &mut tokens) {
            continue;
        }
        push_plain_char(line, &mut index, &mut tokens);
    }
    tokens
}

fn token(text: &str, kind: SyntaxKind) -> Token {
    Token {
        text: text.to_owned(),
        kind,
    }
}

fn consume_until_whitespace(line: &str, start: usize) -> usize {
    for (offset, ch) in line[start..].char_indices() {
        if ch.is_whitespace() {
            return start + offset;
        }
    }
    line.len()
}

fn next_non_space(line: &str, start: usize) -> Option<char> {
    line[start..].chars().find(|ch| !ch.is_whitespace())
}

fn previous_non_space(line: &str, end: usize) -> Option<char> {
    line[..end].chars().rev().find(|ch| !ch.is_whitespace())
}

fn is_identifier_start(ch: char) -> bool {
    ch == '_' || ch.is_alphabetic()
}

fn is_identifier_continue(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

fn is_operator(ch: char) -> bool {
    matches!(
        ch,
        '=' | '+' | '-' | '*' | '/' | '%' | '!' | '<' | '>' | '&' | '|' | '^' | '~' | '?' | ':'
    )
}

fn is_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '.' | '@'
    )
}

fn is_keyword(language: Language, ident: &str) -> bool {
    match language {
        Language::Rust => is_rust_keyword(ident),
        Language::Python => is_python_keyword(ident),
        Language::TypeScriptLike => is_typescript_like_keyword(ident),
        Language::CLike => is_c_like_keyword(ident),
        Language::Shell => is_shell_keyword(ident),
        Language::Json | Language::Toml => false,
    }
}

fn is_rust_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "else"
            | "enum"
            | "extern"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "static"
            | "struct"
            | "trait"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
    )
}

fn is_python_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "and"
            | "as"
            | "assert"
            | "async"
            | "await"
            | "break"
            | "class"
            | "continue"
            | "def"
            | "del"
            | "elif"
            | "else"
            | "except"
            | "finally"
            | "for"
            | "from"
            | "global"
            | "if"
            | "import"
            | "in"
            | "is"
            | "lambda"
            | "nonlocal"
            | "not"
            | "or"
            | "pass"
            | "raise"
            | "return"
            | "try"
            | "while"
            | "with"
            | "yield"
    )
}

fn is_typescript_like_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "async"
            | "await"
            | "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "else"
            | "export"
            | "extends"
            | "finally"
            | "for"
            | "from"
            | "function"
            | "if"
            | "import"
            | "in"
            | "interface"
            | "let"
            | "new"
            | "of"
            | "return"
            | "switch"
            | "throw"
            | "try"
            | "type"
            | "var"
            | "while"
    )
}

fn is_c_like_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "auto"
            | "break"
            | "case"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "else"
            | "enum"
            | "extern"
            | "for"
            | "goto"
            | "if"
            | "inline"
            | "register"
            | "return"
            | "sizeof"
            | "static"
            | "struct"
            | "switch"
            | "typedef"
            | "union"
            | "volatile"
            | "while"
    )
}

fn is_shell_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "case"
            | "do"
            | "done"
            | "elif"
            | "else"
            | "esac"
            | "fi"
            | "for"
            | "function"
            | "if"
            | "in"
            | "then"
            | "while"
    )
}

fn is_constant(language: Language, ident: &str) -> bool {
    match language {
        Language::Rust => matches!(ident, "true" | "false" | "None" | "Some" | "Ok" | "Err"),
        Language::Python => matches!(ident, "True" | "False" | "None"),
        Language::TypeScriptLike => {
            matches!(ident, "true" | "false" | "null" | "undefined" | "NaN")
        }
        Language::CLike => matches!(ident, "NULL" | "true" | "false"),
        Language::Shell => false,
        Language::Json | Language::Toml => matches!(ident, "true" | "false"),
    }
}

fn is_type_name(language: Language, ident: &str) -> bool {
    match language {
        Language::Rust => matches!(
            ident,
            "bool"
                | "char"
                | "f32"
                | "f64"
                | "i8"
                | "i16"
                | "i32"
                | "i64"
                | "i128"
                | "isize"
                | "str"
                | "String"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "u128"
                | "usize"
                | "Vec"
                | "Option"
                | "Result"
        ),
        Language::Python => matches!(
            ident,
            "bool" | "bytes" | "dict" | "float" | "int" | "list" | "set" | "str" | "tuple"
        ),
        Language::TypeScriptLike => matches!(
            ident,
            "any" | "boolean" | "number" | "object" | "string" | "unknown" | "void"
        ),
        Language::CLike => matches!(
            ident,
            "bool"
                | "char"
                | "double"
                | "float"
                | "int"
                | "int16_t"
                | "int32_t"
                | "int64_t"
                | "int8_t"
                | "long"
                | "short"
                | "size_t"
                | "uint16_t"
                | "uint32_t"
                | "uint64_t"
                | "uint8_t"
                | "void"
        ),
        Language::Shell | Language::Json | Language::Toml => false,
    }
}

#[cfg(test)]
mod tests;
