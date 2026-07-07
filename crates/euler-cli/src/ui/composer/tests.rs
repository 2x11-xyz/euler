use super::*;

mod composer_tests {
    use super::*;
    use crate::ui::{
        status::{status_widget, StatusSnapshot, TokenUsageSnapshot, TurnStatus},
        test_backend::VT100Backend,
        theme::Theme,
    };
    use ratatui::{
        layout::{Rect, Size},
        Terminal,
    };
    use std::path::PathBuf;

    #[test]
    fn empty_and_single_line_composer_collapse_to_prompt_row() {
        let draft = ComposerDraft::new();
        let snapshot = ComposerSnapshot::new(&draft);

        let lines = render_lines(&snapshot, &ComposerRenderOptions::default(), 48, 1);
        assert_eq!(
            desired_height(&snapshot, &ComposerRenderOptions::default()),
            1
        );
        assert!(matches!(
            lines.as_slice(),
            [ComposerLine::Draft {
                prompt: true,
                text,
                ..
            }] if text.is_empty()
        ));

        let mut typed = ComposerDraft::new();
        typed.insert_text("hello");
        let typed = ComposerSnapshot::new(&typed);
        assert_eq!(desired_height(&typed, &ComposerRenderOptions::default()), 1);
        assert!(matches!(
            render_lines(&typed, &ComposerRenderOptions::default(), 48, 1).as_slice(),
            [ComposerLine::Draft {
                prompt: true,
                text,
                ..
            }] if text == "hello"
        ));

        let mut paste = ComposerDraft::new();
        paste.insert_bracketed_paste(&"x".repeat(LARGE_PASTE_CHAR_LIMIT + 1));
        let paste = ComposerSnapshot::new(&paste);
        assert_eq!(desired_height(&paste, &ComposerRenderOptions::default()), 1);
        assert!(matches!(
            render_lines(&paste, &ComposerRenderOptions::default(), 48, 1).as_slice(),
            [ComposerLine::Draft { text, .. }] if text.starts_with("[paste #1 ")
        ));
    }

    #[test]
    fn multiline_composer_expands_to_visible_draft_rows() {
        let mut draft = ComposerDraft::new();
        draft.insert_text("one\ntwo\nthree");
        let snapshot = ComposerSnapshot::new(&draft);
        let options = ComposerRenderOptions::default();

        assert_eq!(render_lines(&snapshot, &options, 20, 0), Vec::new());
        assert!(matches!(
            render_lines(&snapshot, &options, 20, 1).as_slice(),
            [ComposerLine::Draft { text, .. }] if text == "three"
        ));
        assert!(matches!(
            render_lines(&snapshot, &options, 20, 2).as_slice(),
            [ComposerLine::Draft { text: one, .. }, ComposerLine::Draft { text: two, .. }]
                if one == "two" && two == "three"
        ));
        assert_eq!(desired_height(&snapshot, &options), 3);
        assert!(matches!(
            render_lines(&snapshot, &options, 20, 3).as_slice(),
            [
                ComposerLine::Draft { text: one, .. },
                ComposerLine::Draft { text: two, .. },
                ComposerLine::Draft { text: three, .. }
            ] if one == "one" && two == "two" && three == "three"
        ));
    }

    #[test]
    fn active_composer_word_wraps_at_boundaries() {
        let mut draft = ComposerDraft::new();
        draft.insert_text("alpha beta gamma");
        let snapshot = ComposerSnapshot::new(&draft);
        let options = ComposerRenderOptions::default();

        assert_eq!(desired_height_for_width(&snapshot, &options, 12), 2);
        assert!(matches!(
            render_lines(&snapshot, &options, 12, 3).as_slice(),
            [
                ComposerLine::Draft { prompt: true, text: first, .. },
                ComposerLine::Draft { prompt: false, text: second, .. },
            ] if first == "alpha beta" && second == "gamma"
        ));
    }

    #[test]
    fn active_composer_hard_wraps_long_unbroken_token() {
        let mut draft = ComposerDraft::new();
        draft.insert_text("abcdefghijk");
        let snapshot = ComposerSnapshot::new(&draft);
        let options = ComposerRenderOptions::default();

        assert!(matches!(
            render_lines(&snapshot, &options, 7, 3).as_slice(),
            [
                ComposerLine::Draft { prompt: true, text: first, .. },
                ComposerLine::Draft { prompt: false, text: second, .. },
                ComposerLine::Draft { prompt: false, text: third, .. },
            ] if first == "abcde" && second == "fghij" && third == "k"
        ));
    }

    #[test]
    fn active_composer_uses_continuous_user_rail_on_wrapped_rows() {
        let mut draft = ComposerDraft::new();
        draft.insert_text("abcdefghij");
        let snapshot = ComposerSnapshot::new(&draft);
        let options = ComposerRenderOptions::default();
        let theme = Theme::default();

        let contents = rendered_composer_and_status(
            &snapshot,
            &StatusSnapshot::new("fixture", "echo", PathBuf::from("/repo")),
            &TokenUsageSnapshot::default(),
            &theme,
            options,
            Size::new(7, 2),
        );

        let rows = contents.lines().take(2).collect::<Vec<_>>();
        assert_eq!(rows[0], "▌ abcde");
        assert_eq!(rows[1], "▌ fghij");
    }

    #[test]
    fn active_composer_uses_continuous_user_rail_on_blank_newline_rows() {
        let mut draft = ComposerDraft::new();
        draft.insert_text("first\n\nthird");
        let snapshot = ComposerSnapshot::new(&draft);
        let options = ComposerRenderOptions::default();
        let theme = Theme::default();

        let contents = rendered_composer_and_status(
            &snapshot,
            &StatusSnapshot::new("fixture", "echo", PathBuf::from("/repo")),
            &TokenUsageSnapshot::default(),
            &theme,
            options,
            Size::new(20, 3),
        );

        let rows = contents.lines().take(3).collect::<Vec<_>>();
        assert_eq!(rows, vec!["▌ first", "▌ ", "▌ third"]);
    }

    #[test]
    fn cursor_position_tracks_wrapped_visual_row() {
        let mut draft = ComposerDraft::new();
        draft.insert_text("abcdefg");
        let options = ComposerRenderOptions::default();

        let position = cursor_position(&draft, 7, &options, 2);

        assert_eq!(position.logical_line, 0);
        assert_eq!(position.visible_row, Some(1));
        assert_eq!(position.column, 4);
    }

    #[test]
    fn cursor_position_at_wrap_boundary_uses_next_visual_row() {
        let mut draft = ComposerDraft::new();
        draft.insert_text("abcdefghij");
        draft.cursor = 5;
        let options = ComposerRenderOptions::default();

        let position = cursor_position(&draft, 7, &options, 2);

        assert_eq!(position.logical_line, 0);
        assert_eq!(position.visible_row, Some(1));
        assert_eq!(position.column, prompt_width(false));
    }

    #[test]
    fn wrapped_paste_placeholder_keeps_atomic_cursor_bounds() {
        let mut draft = ComposerDraft::new();
        draft.insert_bracketed_paste(&"x".repeat(LARGE_PASTE_CHAR_LIMIT + 1));
        let snapshot = ComposerSnapshot::new(&draft);
        let options = ComposerRenderOptions {
            max_visible_lines: 20,
        };
        let rows = render_lines(&snapshot, &options, 8, 20);

        let end_position = cursor_position(&draft, 8, &options, 20);
        draft.cursor = 0;
        let start_position = cursor_position(&draft, 8, &options, 20);

        assert!(rows.len() > 1, "expected wrapped paste label: {rows:?}");
        assert_eq!(start_position.visible_row, Some(0));
        assert_eq!(start_position.column, prompt_width(true));
        assert_eq!(end_position.visible_row, Some(rows.len() - 1));
        assert!(end_position.column > prompt_width(false));
    }

    #[test]
    fn adaptive_composer_scrolls_to_cap_with_overflow_above_statusline() {
        let mut draft = ComposerDraft::new();
        draft.insert_text("line1\nline2\nline3\nline4\nline5\nline6");
        draft.set_scroll_line(2);
        let options = ComposerRenderOptions {
            max_visible_lines: 3,
        };
        let tokens = TokenUsageSnapshot::default();
        let composer = ComposerSnapshot::new(&draft);
        let status = StatusSnapshot::new("fixture", "echo", PathBuf::from("/repo"));
        let theme = Theme::default();
        let height = desired_height(&composer, &options);

        let contents = rendered_composer_and_status(
            &composer,
            &status,
            &tokens,
            &theme,
            options,
            Size::new(64, height),
        );

        assert!(contents.contains("\u{2191} line4"));
        assert!(contents.contains("line5"));
        assert!(contents.contains("line6"));
        assert!(!contents.contains("line1"));
        let screen_lines = contents.lines().collect::<Vec<_>>();
        assert!(screen_lines[usize::from(height)]
            .starts_with("  fixture/echo ? · /repo · Context ?% used"));
    }

    #[test]
    fn paste_marker_render_and_submit_use_hidden_tokens() {
        let payload = (1..=6)
            .map(|line| format!("line{line}"))
            .collect::<Vec<_>>()
            .join("\r\n");
        let mut draft = ComposerDraft::new();

        assert_eq!(
            draft.insert_bracketed_paste(&payload),
            Some(PasteTokenId(1))
        );
        assert_eq!(draft.render_text(), "[paste #1 +6 lines]");
        assert_eq!(
            draft.submit_text(),
            (1..=6)
                .map(|line| format!("line{line}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert_eq!(draft.render_text(), "[paste #1 +6 lines]");

        let mut typed = ComposerDraft::new();
        typed.insert_text("[paste #1 +6 lines]");
        assert_eq!(typed.submit_text(), "[paste #1 +6 lines]");
    }

    #[test]
    fn paste_threshold_boundaries_are_strict_and_payloads_can_mix() {
        let five_lines = (1..=5)
            .map(|line| format!("line{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let five_crlf_lines = (1..=5)
            .map(|line| format!("crlf{line}"))
            .collect::<Vec<_>>()
            .join("\r\n");
        let six_lines = format!("{five_lines}\nline6");
        let thousand_chars = "a".repeat(1_000);
        let thousand_one_chars = "a".repeat(1_001);
        let mut draft = ComposerDraft::new();

        assert_eq!(draft.insert_bracketed_paste(&five_lines), None);
        assert_eq!(draft.insert_bracketed_paste(&five_crlf_lines), None);
        assert_eq!(draft.insert_bracketed_paste(&thousand_chars), None);
        assert_eq!(
            draft.insert_bracketed_paste(&six_lines),
            Some(PasteTokenId(1))
        );
        draft.insert_text(" after ");
        assert_eq!(
            draft.insert_bracketed_paste(&thousand_one_chars),
            Some(PasteTokenId(2))
        );

        assert!(draft.render_text().contains("[paste #1 +6 lines]"));
        assert!(draft.render_text().contains("[paste #2 1001 chars]"));
        assert!(draft.submit_text().contains("line6 after "));
        assert!(draft.submit_text().ends_with(&thousand_one_chars));
    }

    #[test]
    fn canonical_storage_normalizes_crlf_and_cr_to_lf() {
        let mut draft = ComposerDraft::new();
        draft.insert_text("a\r\nb\rc");

        assert_eq!(draft.submit_text(), "a\nb\nc");
        assert!(!draft.submit_text().contains('\r'));
    }

    fn rendered_composer_and_status(
        composer: &ComposerSnapshot<'_>,
        status: &StatusSnapshot,
        tokens: &TokenUsageSnapshot,
        theme: &Theme,
        options: ComposerRenderOptions,
        area: Size,
    ) -> String {
        let backend = VT100Backend::new(area.width, area.height + 1);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| {
                frame.render_widget(
                    composer_widget(composer, theme, options),
                    Rect::new(0, 0, area.width, area.height),
                );
                frame.render_widget(
                    status_widget(status, theme).runtime(tokens, TurnStatus::Idle),
                    Rect::new(0, area.height, area.width, 1),
                );
            })
            .expect("draw");

        terminal.backend().screen_contents()
    }
}
