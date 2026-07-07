use super::super::theme::{BackgroundMode, ColorLevel, ThemeOptions};
use super::*;

fn token_text(tokens: &[Token]) -> String {
    tokens.iter().map(|token| token.text.as_str()).collect()
}

fn token_kind(tokens: &[Token], text: &str) -> SyntaxKind {
    tokens
        .iter()
        .find(|token| token.text == text)
        .unwrap_or_else(|| panic!("missing token {text:?} in {tokens:?}"))
        .kind
}

fn dark_theme_with(color_level: ColorLevel) -> Theme {
    Theme::default_dark_with(ThemeOptions {
        color_level,
        background: BackgroundMode::DEFAULT_DARK_OPAQUE,
    })
}

#[test]
fn rust_tokenizer_is_lossless_and_distinguishes_core_roles() {
    let source = "pub fn main() { println!(\"hi\", 42); // привет }";
    let tokens = tokenize_line(source, Language::Rust);
    assert_eq!(token_text(&tokens), source);
    assert!(tokens.iter().any(|t| t.kind == SyntaxKind::Keyword));
    assert!(tokens.iter().any(|t| t.kind == SyntaxKind::Function));
    assert!(tokens.iter().any(|t| t.kind == SyntaxKind::Macro));
    assert!(tokens.iter().any(|t| t.kind == SyntaxKind::String));
    assert!(tokens.iter().any(|t| t.kind == SyntaxKind::Number));
    assert!(tokens.iter().any(|t| t.kind == SyntaxKind::Comment));
}

#[test]
fn rust_type_annotations_and_not_equal_do_not_become_properties_or_macros() {
    let source = "let x: i32 = 1; if x!=2 { return; }";
    let tokens = tokenize_line(source, Language::Rust);

    assert_eq!(token_text(&tokens), source);
    assert_eq!(token_kind(&tokens, "x"), SyntaxKind::Variable);
    assert!(tokens
        .iter()
        .any(|token| token.text == "!=" && token.kind == SyntaxKind::Operator));
    assert!(!tokens
        .iter()
        .any(|token| token.text == "x!" && token.kind == SyntaxKind::Macro));
}

#[test]
fn number_tokens_stop_before_infix_operators_but_keep_exponent_signs() {
    let source = "let value = 1+2-3.0e-4;";
    let tokens = tokenize_line(source, Language::Rust);

    assert_eq!(token_text(&tokens), source);
    assert!(tokens
        .iter()
        .any(|token| token.text == "1" && token.kind == SyntaxKind::Number));
    assert!(tokens
        .iter()
        .any(|token| token.text == "+" && token.kind == SyntaxKind::Operator));
    assert!(tokens
        .iter()
        .any(|token| token.text == "2" && token.kind == SyntaxKind::Number));
    assert!(tokens
        .iter()
        .any(|token| token.text == "-" && token.kind == SyntaxKind::Operator));
    assert!(tokens
        .iter()
        .any(|token| token.text == "3.0e-4" && token.kind == SyntaxKind::Number));
}

#[test]
fn backtick_strings_are_javascript_only_and_rust_lifetimes_stay_plain() {
    let rust = tokenize_line(
        "let value = `not_rust`; let item: &'a str = \"ok\";",
        Language::Rust,
    );
    let ts = tokenize_line("const value = `template`;", Language::TypeScriptLike);

    assert_eq!(
        token_text(&rust),
        "let value = `not_rust`; let item: &'a str = \"ok\";"
    );
    assert!(!rust
        .iter()
        .any(|token| token.text == "`not_rust`" && token.kind == SyntaxKind::String));
    assert!(rust
        .iter()
        .any(|token| token.text == "\"ok\"" && token.kind == SyntaxKind::String));
    assert_eq!(token_text(&ts), "const value = `template`;");
    assert!(ts
        .iter()
        .any(|token| token.text == "`template`" && token.kind == SyntaxKind::String));
}

#[test]
fn rust_raw_string_is_styled_as_string() {
    let source = r##"let text = r#"hello \"world\""#;"##;
    let tokens = tokenize_line(source, Language::Rust);

    assert_eq!(token_text(&tokens), source);
    assert!(
        tokens
            .iter()
            .any(|token| token.text == r##"r#"hello \"world\""#"##
                && token.kind == SyntaxKind::String)
    );
}

#[test]
fn c_tokenizer_styles_preprocessor_and_plain_code() {
    let include = "#include <stdio.h>";
    let tokens = tokenize_line(include, Language::CLike);
    assert_eq!(token_text(&tokens), include);
    assert_eq!(tokens[0].kind, SyntaxKind::Attribute);

    let source = "static int pick_next(const int dist[]) { return 1; }";
    let tokens = tokenize_line(source, Language::CLike);
    assert_eq!(token_text(&tokens), source);
    assert!(tokens.iter().any(|t| t.kind == SyntaxKind::Keyword));
    assert!(tokens.iter().any(|t| t.kind == SyntaxKind::TypeName));
    assert!(tokens.iter().any(|t| t.kind == SyntaxKind::Function));
}

#[test]
fn json_and_toml_property_tokens_are_lossless() {
    let json = r#""model": "openai/gpt-4.1-mini", "enabled": true"#;
    let json_tokens = tokenize_json_line(json);
    assert_eq!(token_text(&json_tokens), json);
    assert!(json_tokens.iter().any(|t| t.kind == SyntaxKind::Property));
    assert!(json_tokens.iter().any(|t| t.kind == SyntaxKind::String));
    assert!(json_tokens.iter().any(|t| t.kind == SyntaxKind::Constant));

    let toml = r#"model = "openai/gpt-4.1-mini" # configured"#;
    let toml_tokens = tokenize_toml_line(toml);
    assert_eq!(token_text(&toml_tokens), toml);
    assert!(toml_tokens.iter().any(|t| t.kind == SyntaxKind::Property));
    assert!(toml_tokens.iter().any(|t| t.kind == SyntaxKind::Comment));
}

#[test]
fn unicode_and_tabs_stay_lossless() {
    for language in [
        Language::Rust,
        Language::Python,
        Language::TypeScriptLike,
        Language::CLike,
        Language::Shell,
    ] {
        let source = "\tlet café = \"☕\"; // 中";
        assert_eq!(token_text(&tokenize_line(source, language)), source);
    }
}

#[test]
fn json_and_toml_unicode_properties_are_lossless() {
    let json = r#""ключ": "значение", "café": true"#;
    let toml = r#"café = "chaud""#;

    let json_tokens = tokenize_json_line(json);
    let toml_tokens = tokenize_toml_line(toml);

    assert_eq!(token_text(&json_tokens), json);
    assert_eq!(token_text(&toml_tokens), toml);
    assert!(json_tokens
        .iter()
        .any(|token| token.text == r#""ключ""# && token.kind == SyntaxKind::Property));
    assert!(toml_tokens
        .iter()
        .any(|token| token.text == "café" && token.kind == SyntaxKind::Property));
}

#[test]
fn source_pair_budget_counts_newline_terminated_lines_conservatively() {
    let within = "x\n".repeat(MAX_SYNTAX_DIFF_LINES / 2);
    let over = "x\n".repeat(MAX_SYNTAX_DIFF_LINES + 1);

    assert_eq!(source_line_count(""), 0);
    assert_eq!(source_line_count("x"), 1);
    assert_eq!(source_line_count("x\n"), 1);
    assert_eq!(source_line_count("\n\n"), 2);
    assert!(source_pair_within_budget(Some(&within), Some(&within)));
    assert!(!source_pair_within_budget(Some(&over), Some("")));
}

#[test]
fn unknown_or_oversized_lines_fall_back_to_plain_diff_style() {
    let theme = Theme::default();
    let unknown = highlight_diff_body(
        "src/main.unknown",
        "pub fn main() {}",
        DiffBodyKind::Insert,
        &theme,
        true,
    );
    assert_eq!(unknown.len(), 1);
    assert_eq!(unknown[0].style, theme.scopes.diff.inserted_body);

    let long = "x".repeat(MAX_SYNTAX_LINE_BYTES + 1);
    let highlighted = highlight_diff_body("src/main.rs", &long, DiffBodyKind::Insert, &theme, true);
    assert_eq!(highlighted.len(), 1);
    assert_eq!(highlighted[0].style, theme.scopes.diff.inserted_body);
}

#[test]
fn deleted_highlight_keeps_syntax_spans_dimmed_not_flat_red() {
    let theme = Theme::default_light();
    let spans = highlight_diff_body(
        "src/main.rs",
        "pub fn main() { println!(\"hi\"); }",
        DiffBodyKind::Delete,
        &theme,
        true,
    );
    assert!(spans.len() > 1);
    assert!(spans
        .iter()
        .any(|span| span.style.add_modifier.contains(Modifier::DIM)));
    assert!(spans
        .iter()
        .any(|span| span.style.fg != theme.scopes.diff.deleted.fg));
}

#[test]
fn deleted_highlight_dims_lower_color_levels_even_without_rgb_blend() {
    for color_level in [ColorLevel::Indexed256, ColorLevel::Basic16] {
        let theme = dark_theme_with(color_level);
        let spans = highlight_diff_body(
            "src/main.rs",
            "pub fn main() { println!(\"hi\"); }",
            DiffBodyKind::Delete,
            &theme,
            true,
        );

        assert!(spans.len() > 1);
        assert!(spans
            .iter()
            .any(|span| span.style.add_modifier.contains(Modifier::DIM)));
        assert!(spans
            .iter()
            .any(|span| span.style.fg != theme.scopes.diff.deleted.fg));
    }
}

#[test]
fn color_blend_respects_truecolor_only() {
    assert_eq!(
        blend_toward(Color::Rgb(100, 100, 100), Color::Rgb(200, 0, 0), 50),
        Some(Color::Rgb(150, 50, 50))
    );
    assert_eq!(
        blend_toward(Color::Indexed(1), Color::Rgb(200, 0, 0), 50),
        None
    );
    assert_eq!(
        ColorLevel::TrueColor.quantize(Color::Rgb(1, 2, 3)),
        Color::Rgb(1, 2, 3)
    );
}
