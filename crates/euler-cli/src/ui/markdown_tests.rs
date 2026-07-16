use super::*;
use crate::ui::theme::Theme;

fn strings(lines: Vec<Line<'static>>) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| line.spans.into_iter().map(|span| span.content).collect())
        .collect()
}

fn all_spans(lines: &[Line<'static>]) -> Vec<Span<'static>> {
    lines
        .iter()
        .flat_map(|line| line.spans.iter().cloned())
        .collect()
}

#[test]
fn renders_basic_markdown_constructs() {
    let theme = Theme::default_dark();
    let lines = render_agent_markdown(
        "Hello **bold** and *em* with `code`\n\n- one\n1. two\n> quote\n```rust\nlet x = 1;\n```\n",
        &theme,
        80,
    );
    let text = strings(lines);
    assert!(text.iter().any(|line| line.contains("Hello bold and em")));
    assert!(text.iter().any(|line| line.contains("- one")));
    assert!(text.iter().any(|line| line.contains("1. two")));
    assert!(text.iter().any(|line| line.contains("> quote")));
    assert!(text.iter().any(|line| line.contains("let x = 1;")));
}

/// §4.1a: a fenced code block gets one continuous left hairline (`▏`), the
/// code behind it with syntax color, and no background fill of any kind. The
/// language shows as a faint right-corner tag on the first line, never a
/// heading line above the block.
#[test]
fn code_block_uses_a_left_hairline_and_no_background_fill() {
    let theme = Theme::warm_ledger();
    let lines = render_agent_markdown("```sh\neuler run\nsecond line\n```\n", &theme, 40);
    let text = strings(lines.clone());

    // No heading line above the block (the old `    sh` row is gone).
    assert!(
        !text.iter().any(|line| line.trim() == "sh"),
        "language must not be a heading line: {text:?}"
    );
    // Every code row opens with the hairline rail; none is a bare 4-space
    // indent, and the language tag rides the first row's right corner.
    let code_rows: Vec<&String> = text.iter().filter(|line| line.contains('▏')).collect();
    assert_eq!(code_rows.len(), 2, "one rail per code line: {text:?}");
    assert!(code_rows[0].contains("euler run"), "text: {text:?}");
    assert!(
        code_rows[0].trim_end().ends_with("sh"),
        "language tag rides the first code line's right corner: {:?}",
        code_rows[0]
    );
    assert!(!code_rows[1].contains("sh"), "tag only on the first line");

    // No span in the block carries a background fill.
    for span in all_spans(&lines) {
        assert_eq!(span.style.bg, None, "code carries no background: {span:?}");
    }
}

/// §4.1a: inline code is a plain teal color shift (the references role) — no
/// chip, box, or background.
#[test]
fn inline_code_is_teal_with_no_background() {
    let theme = Theme::warm_ledger();
    let lines = render_agent_markdown("run `euler status` now\n", &theme, 80);
    let code = all_spans(&lines)
        .into_iter()
        .find(|span| span.content.contains("euler status"))
        .expect("inline code span");
    assert_eq!(
        code.style.fg,
        Some(theme.palette.tool),
        "inline code is teal"
    );
    assert_eq!(code.style.bg, None, "inline code has no background chip");
}

#[test]
fn wrapped_list_continuations_stay_under_item_text() {
    let theme = Theme::default_dark();
    let lines = render_agent_markdown(
        "- crates/euler-provider: provider abstraction plus implementations for chatgpt openrouter anthropic and sse support\n",
        &theme,
        44,
    );
    let text = strings(lines);

    assert!(text[0].starts_with("- crates/euler-provider:"));
    assert!(
        text.iter()
            .skip(1)
            .filter(|line| !line.is_empty())
            .all(|line| line.starts_with("  ")),
        "wrapped list rows should keep continuation indent: {text:?}"
    );
}

#[test]
fn renders_pipe_tables_and_unwraps_markdown_fenced_tables() {
    let theme = Theme::default_dark();
    let lines = render_agent_markdown(
        "```markdown\n| A | B |\n|---|---|\n| 1 | 2222 |\n| 333 | 4 |\n```\n",
        &theme,
        80,
    );
    let text = strings(lines);
    assert!(text.iter().any(|line| line.contains("A")));
    assert_eq!(
        text.iter().filter(|line| line.contains('─')).count(),
        1,
        "expected exactly one rule line under the header: {text:?}"
    );
    assert!(!text.iter().any(|line| line.contains('━')));
    assert!(text.iter().any(|line| line.contains("2222")));
    assert!(!text.iter().any(|line| line.contains('|')));
    assert!(!text.iter().any(|line| line.contains("```")));
}

#[test]
fn table_cells_wrap_instead_of_truncating_content() {
    let theme = Theme::default_dark();
    let lines = render_agent_markdown(
        "| Area | Purpose |\n|---|---|\n| crates/euler-cli | Command-line entrypoint and user-facing flows for auth login model preference and transcript composition |\n",
        &theme,
        48,
    );
    let text = strings(lines).join("\n");

    assert!(text.contains("Command-line"));
    assert!(text.contains("transcript"));
    assert!(text.contains("composition"));
    assert!(!text.contains("compositio\n"));
}

#[test]
fn two_column_tables_use_grid_until_width_is_too_narrow() {
    let theme = Theme::default_dark();
    let source = "| Area | Purpose |\n|---|---|\n| CLI | Terminal transcript UX |\n";

    let wide = strings(render_agent_markdown(source, &theme, 44));
    assert!(wide.iter().any(|line| line.contains('─')));
    assert!(wide.iter().any(|line| line.contains("CLI")));

    let narrow = strings(render_agent_markdown(source, &theme, 43));
    assert!(narrow.iter().any(|line| line == "Area: CLI"));
    assert!(narrow
        .iter()
        .any(|line| line == "Purpose: Terminal transcript UX"));
    assert!(!narrow.iter().any(|line| line.contains('─')));
}

#[test]
fn narrow_multi_column_tables_stack_as_label_value_rows() {
    let theme = Theme::default_dark();
    let lines = render_agent_markdown(
        "| Layer | Responsibility | Repo location |\n|---|---|---|\n| CLI/TUI layer | User-facing command-line and Ratatui transcript composer status UX | euler-cli |\n| Provider layer | Normalize LLM provider APIs into common ModelProvider abstraction | euler-provider |\n",
        &theme,
        44,
    );
    let text = strings(lines);

    assert!(text.iter().any(|line| line == "Layer: CLI/TUI layer"));
    assert!(text.iter().any(|line| line == "Repo location: euler-cli"));
    assert!(text.iter().any(|line| {
        line.contains("User-facing command-line") || line.trim_start().starts_with("and Ratatui")
    }));
    assert!(!text.iter().any(|line| line.contains('━')));
    assert!(!text.iter().any(|line| line.contains('─')));
    assert!(
        text.iter().all(|line| display_width(line) <= 44),
        "narrow stacked table overflowed: {text:?}"
    );
}

#[test]
fn multi_column_tables_use_grid_at_wide_widths() {
    let theme = Theme::default_dark();
    let lines = render_agent_markdown(
        "| Layer | Responsibility | Repo location |\n|---|---|---|\n| Event substrate | Stable event envelope and kind taxonomy | euler-event |\n| Core engine | Session loop and provenance | euler-core |\n",
        &theme,
        100,
    );
    let text = strings(lines);

    assert!(text
        .iter()
        .any(|line| line.contains("Layer") && line.contains("Responsibility")));
    assert!(text
        .iter()
        .any(|line| line.contains("Event substrate") && line.contains("euler-event")));
    assert!(text
        .iter()
        .any(|line| line.contains("Core engine") && line.contains("euler-core")));
    assert_eq!(
        text.iter().filter(|line| line.contains('─')).count(),
        1,
        "expected exactly one rule line under the header: {text:?}"
    );
    assert!(!text.iter().any(|line| line.contains('━')));
    assert!(!text.iter().any(|line| line == "Layer: Event substrate"));
    assert!(
        text.iter().all(|line| display_width(line) <= 100),
        "wide grid table overflowed: {text:?}"
    );
}

#[test]
fn five_column_tables_grid_only_when_width_is_sufficient() {
    let theme = Theme::default_dark();
    let source =
        "| A | B | C | D | E |\n|---|---|---|---|---|\n| one | two | three | four | five |\n";

    let wide = strings(render_agent_markdown(source, &theme, 110));
    assert!(wide
        .iter()
        .any(|line| line.contains("one") && line.contains("five")));
    assert!(wide.iter().any(|line| line.contains('─')));
    assert!(!wide.iter().any(|line| line == "A: one"));
    assert!(
        wide.iter().all(|line| display_width(line) <= 110),
        "wide five-column grid overflowed: {wide:?}"
    );

    let narrow = strings(render_agent_markdown(source, &theme, 109));
    assert!(narrow.iter().any(|line| line == "A: one"));
    assert!(narrow.iter().any(|line| line == "E: five"));
    assert!(!narrow.iter().any(|line| line.contains('─')));
    assert!(
        narrow.iter().all(|line| display_width(line) <= 109),
        "narrow five-column stack overflowed: {narrow:?}"
    );
}

#[test]
fn excessive_column_tables_stack_even_when_terminal_is_wide() {
    let theme = Theme::default_dark();
    let lines = render_agent_markdown(
        "| A | B | C | D | E | F |\n|---|---|---|---|---|---|\n| one | two | three | four | five | six |\n",
        &theme,
        160,
    );
    let text = strings(lines);

    assert!(text.iter().any(|line| line == "A: one"));
    assert!(text.iter().any(|line| line == "F: six"));
    assert!(!text.iter().any(|line| line.contains('━')));
    assert!(!text.iter().any(|line| line.contains('─')));
}

#[test]
fn stacked_tables_handle_long_labels_empty_cells_and_inline_text() {
    let theme = Theme::default_dark();
    let lines = render_agent_markdown(
        "| Extremely verbose responsibility label | Notes | Empty |\n|---|---|---|\n| `code` and **strong** text | wraps cleanly | |\n",
        &theme,
        20,
    );
    let text = strings(lines);

    assert!(text.iter().any(|line| line.contains("code")));
    assert!(text.iter().any(|line| line.contains("strong")));
    assert!(!text.iter().any(|line| line.contains("Empty:")));
    assert!(
        text.iter().all(|line| display_width(line) <= 20),
        "stacked table with long labels overflowed: {text:?}"
    );
}

#[test]
fn header_only_multi_column_table_does_not_self_label_cells() {
    let theme = Theme::default_dark();
    let lines = render_agent_markdown("| A | B | C |\n|---|---|---|\n", &theme, 80);
    let text = strings(lines);

    assert!(!text.iter().any(|line| line == "A: A"));
    assert!(!text.iter().any(|line| line == "B: B"));
    assert!(!text.iter().any(|line| line == "C: C"));
}

#[test]
fn unwraps_tilde_markdown_fenced_tables() {
    let theme = Theme::default_dark();
    let lines = render_agent_markdown(
        "~~~markdown\n| A | B |\n|---|---|\n| 1 | 2 |\n~~~\n",
        &theme,
        80,
    );
    let text = strings(lines);
    assert!(text.iter().any(|line| line.contains("A")));
    assert!(text.iter().any(|line| line.contains('─')));
    assert!(!text.iter().any(|line| line.contains('|')));
    assert!(!text.iter().any(|line| line.contains("~~~")));
}

#[test]
fn keeps_non_table_markdown_fence_as_code() {
    let normalized = unwrap_markdown_fences("```md\n**bold**\n```\n");
    assert_eq!(normalized, "```md\n**bold**\n```\n");
}

#[test]
fn markdown_fence_with_blank_between_header_and_delimiter_is_not_unwrapped() {
    let src = "```markdown\n| A | B |\n\n|---|---|\n| 1 | 2 |\n```\n";
    assert_eq!(unwrap_markdown_fences(src), src);
}

#[test]
fn table_alignment_uses_pulldown_alignment() {
    let theme = Theme::default_dark();
    let lines = render_agent_markdown("| C | R |\n|:---:|---:|\n| z | 7 |\n", &theme, 80);
    let text = strings(lines);
    let body = text
        .iter()
        .find(|line| line.contains('z') && line.contains('7'))
        .expect("body row should render");
    assert!(body.contains("  7"));
    assert!(body.contains(" z "));
}

#[test]
fn bare_fence_with_table_is_not_unwrapped() {
    let src = "```\n| A | B |\n|---|---|\n| 1 | 2 |\n```\n";
    assert_eq!(unwrap_markdown_fences(src), src);
}

#[test]
fn unclosed_markdown_fence_with_table_is_not_unwrapped() {
    let src = "```markdown\n| A | B |\n|---|---|\n| 1 | 2 |\n";
    assert_eq!(unwrap_markdown_fences(src), src);
}

#[test]
fn nested_markdown_fence_with_table_is_not_unwrapped() {
    let src = "````markdown\n```text\n| A | B |\n|---|---|\n```\n````\n";
    assert_eq!(unwrap_markdown_fences(src), src);
}

#[test]
fn wrapped_paragraph_preserves_inline_span_styles() {
    let theme = Theme::default_dark();
    let lines = render_agent_markdown("aa **bold** *em* `code` zz", &theme, 11);
    assert!(lines.len() > 1);

    let spans = all_spans(&lines);
    assert!(spans
        .iter()
        .any(|span| span.content.contains("bold") && span.style == theme.scopes.markup.strong));
    assert!(spans
        .iter()
        .any(|span| span.content.contains("em") && span.style == theme.scopes.markup.emphasis));
    assert!(spans
        .iter()
        .any(|span| span.content.contains("code") && span.style == theme.scopes.markup.code));
}

#[test]
fn wrapped_styled_markdown_round_trips_text_across_widths() {
    let theme = Theme::default_dark();
    let expected = "namedresumearchitectureboundary";

    for width in 4..=18 {
        let lines =
            render_agent_markdown("named **resume** *architecture* `boundary`", &theme, width);
        let rendered = strings(lines)
            .join("")
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>();

        assert_eq!(rendered, expected, "width {width}");
    }
}

#[test]
fn wraps_on_word_boundaries_when_styled_spans_allow_it() {
    let theme = Theme::default_dark();
    let lines = strings(render_agent_markdown(
        "named **resume** architecture",
        &theme,
        12,
    ));

    assert_eq!(lines, vec!["named resume", "architecture"]);
}

#[test]
fn table_v1_flattens_cell_styles_at_truncation_boundary() {
    let theme = Theme::default_dark();
    let lines = render_agent_markdown("| A |\n|---|\n| `code` |\n", &theme, 20);
    let spans = all_spans(&lines);

    assert!(spans.iter().any(|span| span.content.contains("code")));
    assert!(!spans
        .iter()
        .any(|span| span.content.contains("code") && span.style == theme.scopes.markup.code));
}

#[test]
fn grid_table_has_one_blank_line_between_data_rows_and_no_rule_above_or_below() {
    // Review v2 §10/10b: only the header separator renders — nothing above
    // the header, nothing after the last row — and one blank line separates
    // each pair of data rows so wrapped cells still read as a single block.
    let theme = Theme::default_dark();
    let lines = strings(render_agent_markdown(
        "| Layer | Responsibility |\n|---|---|\n| CLI | Terminal UX |\n| Core | Session loop |\n| Provider | LLM APIs |\n",
        &theme,
        60,
    ));

    assert_eq!(
        lines.iter().filter(|line| line.contains('─')).count(),
        1,
        "exactly one rule (the header separator): {lines:?}"
    );
    assert!(
        !lines[0].contains('─'),
        "no rule above the header: {lines:?}"
    );
    assert!(
        !lines.last().is_some_and(|line| line.contains('─')),
        "no rule after the last row: {lines:?}"
    );

    let cli_row = lines
        .iter()
        .position(|line| line.contains("CLI"))
        .expect("CLI row");
    let core_row = lines
        .iter()
        .position(|line| line.contains("Core"))
        .expect("Core row");
    let provider_row = lines
        .iter()
        .position(|line| line.contains("Provider"))
        .expect("Provider row");

    assert_eq!(
        lines[cli_row + 1].trim(),
        "",
        "a blank line separates the first two data rows: {lines:?}"
    );
    assert_eq!(core_row, cli_row + 2, "rows: {lines:?}");
    assert_eq!(
        lines[core_row + 1].trim(),
        "",
        "a blank line separates the next two data rows: {lines:?}"
    );
    assert_eq!(provider_row, core_row + 2, "rows: {lines:?}");
}

#[test]
fn grid_table_header_is_cream_bold_and_first_column_is_dim() {
    // Review v2 §10b: header text renders cream bold; the first column
    // (a row label) renders dim, including in the header row's other
    // columns which stay bold.
    let theme = Theme::default_dark();
    let lines = render_agent_markdown(
        "| Layer | Responsibility |\n|---|---|\n| CLI | Terminal UX |\n",
        &theme,
        60,
    );
    let spans = all_spans(&lines);

    let header_first_col = spans
        .iter()
        .find(|span| span.content.trim() == "Layer")
        .expect("header first-column span");
    assert_eq!(
        header_first_col.style,
        theme
            .transcript
            .assistant
            .add_modifier(ratatui::style::Modifier::BOLD),
        "header row stays cream bold across every column"
    );

    let body_first_col = spans
        .iter()
        .find(|span| span.content.trim() == "CLI")
        .expect("body first-column span");
    assert_eq!(
        body_first_col.style, theme.transcript.muted,
        "the first column reads as a row label — dim"
    );

    let body_second_col = spans
        .iter()
        .find(|span| span.content.contains("Terminal UX"))
        .expect("body second-column span");
    assert_eq!(
        body_second_col.style, theme.transcript.assistant,
        "non-first columns keep the plain body style"
    );
}
