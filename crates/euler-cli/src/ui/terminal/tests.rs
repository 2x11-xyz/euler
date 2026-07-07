use super::*;

mod terminal_tests {
    use super::*;
    use crate::ui::test_backend::VT100Backend;
    use ratatui::style::{Color, Modifier};
    use std::sync::Mutex;

    static TERMINAL_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn terminal_owner_rejects_second_enter_before_terminal_setup() {
        let _guard = TERMINAL_TEST_LOCK.lock().expect("terminal test lock");
        let first = acquire_terminal_owner().expect("first owner");

        let error = acquire_terminal_owner().expect_err("second owner should fail");

        assert!(error.to_string().contains("already active"));
        drop(first);
        acquire_terminal_owner().expect("owner released");
    }

    #[test]
    fn pending_signal_take_prioritizes_terminate_and_consumes_one_signal_at_a_time() {
        let _guard = TERMINAL_TEST_LOCK.lock().expect("terminal test lock");
        SIGINT_PENDING.store(false, Ordering::SeqCst);
        SIGTERM_PENDING.store(false, Ordering::SeqCst);

        SIGINT_PENDING.store(true, Ordering::SeqCst);
        SIGTERM_PENDING.store(true, Ordering::SeqCst);

        assert_eq!(take_pending_signal(), Some(PendingSignal::Terminate));
        assert_eq!(take_pending_signal(), Some(PendingSignal::Interrupt));
        assert_eq!(take_pending_signal(), None);
    }

    #[test]
    fn signal_bridge_is_session_scoped_and_reentrant_after_drop() {
        let _guard = TERMINAL_TEST_LOCK.lock().expect("terminal test lock");
        SIGINT_PENDING.store(false, Ordering::SeqCst);
        SIGTERM_PENDING.store(false, Ordering::SeqCst);

        let first = install_signal_bridge().expect("first bridge");
        SIGINT_PENDING.store(true, Ordering::SeqCst);
        drop(first);
        assert_eq!(take_pending_signal(), None);

        let second = install_signal_bridge().expect("bridge can be reinstalled");
        drop(second);
    }

    #[test]
    fn terminal_session_modes_emit_bracketed_paste_without_mouse_capture() {
        let mut enter = Vec::new();
        enable_terminal_session_modes(&mut enter).expect("enable modes");
        assert!(
            enter.windows(8).any(|window| window == b"\x1b[?2004h"),
            "enter bytes: {enter:?}"
        );
        assert!(
            !enter.windows(8).any(|window| window == b"\x1b[?1000h")
                && !enter.windows(8).any(|window| window == b"\x1b[?1006h"),
            "enter bytes must not enable mouse capture because it blocks native selection: {enter:?}"
        );
        assert!(
            enter.windows(6).any(|window| window == b"\x1b[?25l"),
            "enter bytes: {enter:?}"
        );

        let mut restore = Vec::new();
        restore_terminal_session_modes(&mut restore).expect("restore modes");
        assert!(
            restore.windows(8).any(|window| window == b"\x1b[?2004l"),
            "restore bytes: {restore:?}"
        );
        assert!(
            !restore.windows(8).any(|window| window == b"\x1b[?1006l")
                && !restore.windows(8).any(|window| window == b"\x1b[?1000l"),
            "restore bytes should not need mouse-capture cleanup: {restore:?}"
        );
        assert!(
            restore.windows(6).any(|window| window == b"\x1b[?25h"),
            "restore bytes: {restore:?}"
        );
    }

    #[test]
    fn visual_frame_uses_required_rows_from_top_packed_startup() {
        let backend = VT100Backend::new(80, 20);
        let mut terminal = InlineTerminal::new(backend, 16).expect("inline terminal");
        let frame = VisualCanvasFrame {
            active_frame_lines: vec![CanvasLine::plain("▌"), CanvasLine::plain("status")],
            cursor: None,
            required_height: 2,
            history_rows: 0,
            prefer_stable_height: false,
            committable_rows: 0,
            pinned_rows: 0,
        };

        terminal.draw_visual_frame(&frame).expect("draw frame");

        assert_eq!(terminal.viewport_area().y, 0);
        assert_eq!(terminal.viewport_area().height, 2);
        let rows = terminal.backend().screen_rows();
        assert!(rows[0].contains("▌"), "rows: {rows:?}");
        assert!(rows[1].contains("status"), "rows: {rows:?}");
    }

    #[test]
    fn visual_frame_shows_cursor_only_when_frame_has_cursor_target() {
        let backend = VT100Backend::new(30, 6);
        let mut terminal = InlineTerminal::new(backend, 6).expect("inline terminal");
        terminal.backend_mut().clear_raw_output();

        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![CanvasLine::plain("▌ draft")],
                cursor: Some(CursorTarget { row: 0, column: 3 }),
                required_height: 1,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw cursor frame");
        let raw = terminal.backend().raw_output();
        assert!(
            raw.windows(6).any(|window| window == b"\x1b[?25h"),
            "focused frame should show cursor: {raw:?}"
        );
        let hide = find_bytes(raw, b"\x1b[?25l").expect("focused frame should hide before draw");
        let show = find_bytes(raw, b"\x1b[?25h").expect("focused frame should show after draw");
        let final_move =
            find_bytes(raw, b"\x1b[1;4H").expect("focused frame should move to final cursor");
        assert!(
            hide < final_move,
            "cursor hide should precede final move: {raw:?}"
        );
        assert!(
            final_move < show,
            "cursor show should follow final move: {raw:?}"
        );

        terminal.backend_mut().clear_raw_output();
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![CanvasLine::plain("working")],
                cursor: None,
                required_height: 1,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw non-cursor frame");
        let raw = terminal.backend().raw_output();
        assert!(
            raw.windows(6).any(|window| window == b"\x1b[?25l"),
            "non-focused frame should hide cursor: {raw:?}"
        );
    }

    #[test]
    fn prompt_role_uses_user_accent_style() {
        let backend = VT100Backend::new(30, 3);
        let mut terminal = InlineTerminal::new(backend, 3).expect("inline terminal");
        let prompt = CanvasSpan::new_lossy("▌ ", TextRole::Prompt);
        assert_eq!(canvas_span_style(&prompt).fg, Some(USER_RAIL_COLOR));
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![CanvasLine::plain("placeholder")],
                cursor: None,
                required_height: 1,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("prime draw cache");
        terminal.backend_mut().clear_raw_output();

        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![CanvasLine::from_spans(vec![
                    prompt,
                    CanvasSpan::new_lossy("draft", TextRole::Plain),
                ])],
                cursor: None,
                required_height: 1,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw prompt frame");

        let raw = terminal.backend().raw_output();
        assert!(
            raw.windows(4).any(|window| window == b"\x1b[1m"),
            "prompt rail should keep the visible bold affordance: {raw:?}"
        );
    }

    #[test]
    fn canvas_span_style_preserves_explicit_reset_and_removed_modifiers() {
        let span = CanvasSpan::styled_lossy(
            "plain prompt",
            TextRole::Prompt,
            Style::default()
                .fg(RatatuiColor::Reset)
                .remove_modifier(Modifier::BOLD),
        );

        let style = canvas_span_style(&span);
        assert_eq!(style.fg, Some(RatatuiColor::Reset));
        assert!(!style.add_modifier.contains(Modifier::BOLD));
        assert!(style.sub_modifier.contains(Modifier::BOLD));
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn count_bytes_before(haystack: &[u8], needle: &[u8], end: usize) -> usize {
        haystack[..end]
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count()
    }

    #[test]
    fn startup_claims_top_viewport_when_shell_cursor_is_bottom_row() {
        let mut backend = VT100Backend::new(80, 20);
        backend
            .write_all(b"\x1b[20;1Hshell prompt")
            .expect("place shell prompt on bottom row");
        backend.clear_raw_output();
        let mut terminal = InlineTerminal::new(backend, 16).expect("inline terminal");
        let raw_startup = terminal.backend().raw_output();
        assert!(
            raw_startup.windows(4).any(|window| window == b"\x1b[3J"),
            "raw startup bytes: {raw_startup:?}"
        );
        assert!(
            !raw_startup
                .windows(8)
                .any(|window| matches!(window, b"\x1b[?1049h" | b"\x1b[?1049l")),
            "raw startup bytes: {raw_startup:?}"
        );
        let frame = VisualCanvasFrame {
            active_frame_lines: vec![CanvasLine::plain("▌"), CanvasLine::plain("status")],
            cursor: None,
            required_height: 2,
            history_rows: 0,
            prefer_stable_height: false,
            committable_rows: 0,
            pinned_rows: 0,
        };

        terminal.draw_visual_frame(&frame).expect("draw frame");

        assert_eq!(terminal.viewport_area().y, 0);
        let rows = terminal.backend().screen_rows();
        assert!(rows[0].contains("▌"), "rows: {rows:?}");
        assert!(rows[1].contains("status"), "rows: {rows:?}");
        assert!(
            !rows.iter().any(|row| row.contains("shell prompt")),
            "rows: {rows:?}"
        );
    }

    #[test]
    fn visible_active_lines_follow_tail_without_resizing_content() {
        let lines = vec![
            CanvasLine::plain("old"),
            CanvasLine::plain("answer"),
            CanvasLine::plain("▌"),
            CanvasLine::plain("status"),
        ];

        let visible = visible_active_lines(&lines, 2, 0, 0);

        assert_eq!(visible.prefix_start, 2);
        assert_eq!(
            visible
                .lines
                .iter()
                .map(CanvasLine::text)
                .collect::<Vec<_>>(),
            vec!["▌", "status"]
        );
    }

    #[test]
    fn review_scroll_offset_shows_earlier_active_rows() {
        let lines = vec![
            CanvasLine::plain("old"),
            CanvasLine::plain("answer"),
            CanvasLine::plain("▌"),
            CanvasLine::plain("status"),
        ];

        let visible = visible_active_lines(&lines, 2, 2, 0);

        assert_eq!(visible.prefix_start, 0);
        assert_eq!(
            visible
                .lines
                .iter()
                .map(CanvasLine::text)
                .collect::<Vec<_>>(),
            vec!["old", "answer"]
        );
    }

    #[test]
    fn review_scroll_keeps_pinned_suffix_visible() {
        let lines = vec![
            CanvasLine::plain("old"),
            CanvasLine::plain("answer"),
            CanvasLine::plain("streaming"),
            CanvasLine::plain("▌ next draft"),
            CanvasLine::plain("status"),
        ];

        let visible = visible_active_lines(&lines, 4, 2, 2);

        assert_eq!(visible.prefix_start, 0);
        assert_eq!(
            visible
                .lines
                .iter()
                .map(CanvasLine::text)
                .collect::<Vec<_>>(),
            vec!["old", "answer", "▌ next draft", "status"]
        );
        assert_eq!(
            visible_cursor(Some(CursorTarget { row: 3, column: 3 }), &visible),
            Some(CursorTarget { row: 2, column: 3 })
        );
    }

    #[test]
    fn review_scroll_cursor_remains_visible_in_pinned_suffix() {
        let backend = VT100Backend::new(50, 4);
        let mut terminal = InlineTerminal::new(backend, 4).expect("inline terminal");
        terminal.set_review_scroll_offset(8);

        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("old"),
                    CanvasLine::plain("answer"),
                    CanvasLine::plain("streaming"),
                    CanvasLine::plain("▌ next draft"),
                    CanvasLine::plain("status"),
                ],
                cursor: Some(CursorTarget { row: 3, column: 3 }),
                required_height: 5,
                history_rows: 3,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 2,
            })
            .expect("draw reviewed frame");

        let rows = terminal.backend().screen_rows();
        assert!(rows[2].contains("▌ next draft"), "rows: {rows:?}");
        let raw = terminal.backend().raw_output();
        assert!(
            raw.windows(6).any(|window| window == b"\x1b[?25h"),
            "reviewed pinned composer should show cursor: {raw:?}"
        );
        assert!(
            raw.windows(6).any(|window| window == b"\x1b[3;4H"),
            "cursor should move to pinned composer row while review-scrolled: {raw:?}"
        );
    }

    #[test]
    fn finalized_lines_wrap_without_truncating_content() {
        let backend = VT100Backend::new(5, 5);
        let mut terminal = InlineTerminal::new(backend, 5).expect("inline terminal");

        terminal
            .write_finalized_lines(&[CanvasLine::plain("abcdeab\u{754c}cde")])
            .expect("write finalized");

        let rows = trimmed_rows(terminal.backend());
        assert_eq!(&rows[..3], ["abcde", "ab\u{754c}c", "de"]);
    }

    #[test]
    fn finalized_canvas_wrapping_round_trips_styled_span_boundaries() {
        let line = CanvasLine::from_spans(vec![
            CanvasSpan::styled_lossy("named", TextRole::Plain, Style::default().fg(Color::Blue)),
            CanvasSpan::styled_lossy(" resume", TextRole::Plain, Style::default().fg(Color::Red)),
            CanvasSpan::styled_lossy(
                " architecture",
                TextRole::Plain,
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]);
        let expected = line.plain_text();

        for width in 1..=18 {
            let wrapped = wrap_canvas_line(&line, width);
            let rendered = wrapped
                .iter()
                .map(CanvasLine::plain_text)
                .collect::<String>();

            assert_eq!(rendered, expected, "width {width}");
        }
    }

    #[test]
    fn finalized_write_round_trips_content_across_widths() {
        let source = "named resume architecture";
        for width in 4..=12 {
            let backend = VT100Backend::new(width, 16);
            let mut terminal = InlineTerminal::new(backend, 16).expect("inline terminal");

            terminal
                .write_finalized_lines(&[CanvasLine::plain(source)])
                .expect("write finalized");

            let rendered = trimmed_rows(terminal.backend())
                .into_iter()
                .take_while(|row| !row.trim().is_empty())
                .collect::<String>()
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>();
            assert_eq!(rendered, "namedresumearchitecture", "width {width}");
        }
    }

    #[test]
    fn canvas_roles_and_span_styles_convert_to_distinct_ratatui_styles() {
        let line = CanvasLine::from_spans(vec![
            CanvasSpan::new("prompt", TextRole::Prompt),
            CanvasSpan::new("status", TextRole::Status),
            CanvasSpan::styled_lossy(
                "error",
                TextRole::Plain,
                Style::default()
                    .fg(Color::Red)
                    .add_modifier(Modifier::UNDERLINED),
            ),
        ]);

        let converted = canvas_lines_to_ratatui(&[line]);

        let spans = &converted[0].spans;
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(spans[1].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(spans[2].style.fg, Some(Color::Red));
        assert!(spans[2].style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn finalized_write_after_multirow_active_frame_leaves_no_stale_rows() {
        let backend = VT100Backend::new(20, 6);
        let mut terminal = InlineTerminal::new(backend, 6).expect("inline terminal");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("live-one"),
                    CanvasLine::plain("live-two"),
                    CanvasLine::plain("live-three"),
                ],
                cursor: None,
                required_height: 3,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw active");

        terminal
            .write_finalized_lines(&[CanvasLine::plain("final-A"), CanvasLine::plain("final-B")])
            .expect("write finalized");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![CanvasLine::plain("next"), CanvasLine::plain("status")],
                cursor: None,
                required_height: 2,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw next active");

        let rows = trimmed_rows(terminal.backend());
        assert_eq!(&rows[..4], ["final-A", "final-B", "next", "status"]);
        assert!(
            rows.iter().all(|row| !row.contains("live-")),
            "rows: {rows:?}"
        );
    }

    #[test]
    fn scrolled_history_prefix_is_committed_to_native_scrollback_once() {
        let backend = VT100Backend::new(30, 4);
        let mut terminal = InlineTerminal::new(backend, 3).expect("inline terminal");
        let frame = VisualCanvasFrame {
            active_frame_lines: vec![
                CanvasLine::plain("history-one"),
                CanvasLine::plain("history-two"),
                CanvasLine::plain("▌ prompt"),
                CanvasLine::plain("status"),
            ],
            cursor: None,
            required_height: 4,
            history_rows: 2,
            prefer_stable_height: false,
            committable_rows: 2,
            pinned_rows: 0,
        };

        terminal.draw_visual_frame(&frame).expect("draw frame");
        terminal.draw_visual_frame(&frame).expect("redraw frame");

        let rows = terminal.backend().scrollback_rows();
        assert_ordered_rows(&rows, &["history-one", "history-two", "▌ prompt", "status"]);
        assert_eq!(
            rows.iter()
                .filter(|row| row.contains("history-one"))
                .count(),
            1,
            "rows: {rows:?}"
        );
    }

    #[test]
    fn reset_for_history_replay_without_purge_preserves_scrollback_buffer() {
        let backend = VT100Backend::new(30, 4);
        let mut terminal = InlineTerminal::new(backend, 3).expect("inline terminal");

        terminal.backend_mut().clear_raw_output();
        terminal
            .reset_for_history_replay(false)
            .expect("reset for history replay");
        let reset_raw = terminal.backend().raw_output();

        assert!(
            !reset_raw
                .windows(b"\x1b[3J".len())
                .any(|window| window == b"\x1b[3J"),
            "plain replay reset must not purge native scrollback bytes: {reset_raw:?}"
        );
    }

    #[test]
    fn terminal_sequence_writes_through_owned_backend() {
        let backend = VT100Backend::new(30, 4);
        let mut terminal = InlineTerminal::new(backend, 3).expect("inline terminal");

        terminal.backend_mut().clear_raw_output();
        terminal
            .write_terminal_sequence("\x1b]52;c;Y29weQ==\x07")
            .expect("terminal sequence");

        assert_eq!(terminal.backend().raw_output(), b"\x1b]52;c;Y29weQ==\x07");
    }

    #[test]
    fn terminal_theme_colors_emit_gruvbox_cursor_color() {
        let backend = VT100Backend::new(30, 4);
        let mut terminal = InlineTerminal::new(backend, 3).expect("inline terminal");

        terminal.backend_mut().clear_raw_output();
        terminal
            .set_theme_colors(
                RatatuiColor::Rgb(60, 56, 54),
                RatatuiColor::Rgb(251, 241, 199),
                RatatuiColor::Rgb(60, 56, 54),
            )
            .expect("theme colors");

        assert_eq!(terminal.backend().raw_output(), b"\x1b]12;#3c3836\x07");
    }

    #[test]
    fn restore_terminal_modes_reset_cursor_color() {
        let mut restore = Vec::new();

        restore_terminal_session_modes(&mut restore).expect("restore modes");

        assert!(
            restore.windows(5).any(|window| window == b"\x1b]112"),
            "restore bytes should reset cursor color: {restore:?}"
        );
    }

    #[test]
    fn reset_for_history_replay_with_purge_resets_commit_watermark() {
        let backend = VT100Backend::new(30, 4);
        let mut terminal = InlineTerminal::new(backend, 3).expect("inline terminal");
        terminal.set_linefeed_history_insert_enabled(true);
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("history-one"),
                    CanvasLine::plain("history-two"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 4,
                history_rows: 2,
                prefer_stable_height: false,
                committable_rows: 2,
                pinned_rows: 0,
            })
            .expect("initial commit");
        assert!(terminal
            .backend()
            .scrollback_rows()
            .iter()
            .any(|row| row.contains("history-one")));

        terminal.backend_mut().clear_raw_output();
        terminal
            .reset_for_history_replay(true)
            .expect("reset for history replay");
        let reset_raw = terminal.backend().raw_output();
        assert!(
            reset_raw
                .windows(b"\x1b[3J".len())
                .any(|window| window == b"\x1b[3J"),
            "replay reset must purge stale native scrollback bytes: {reset_raw:?}"
        );
        terminal.backend_mut().clear_raw_output();
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("history-one"),
                    CanvasLine::plain("history-two"),
                    CanvasLine::plain("history-three"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 5,
                history_rows: 3,
                prefer_stable_height: false,
                committable_rows: 3,
                pinned_rows: 0,
            })
            .expect("replay commit");
        let raw = terminal.backend().raw_output();
        assert!(
            raw.windows("history-one".len())
                .any(|window| window == b"history-one"),
            "replay should be able to commit source rows after reset: {raw:?}"
        );
    }

    #[test]
    fn pinned_suffix_history_commit_waits_until_review_returns_to_tail() {
        let backend = VT100Backend::new(30, 4);
        let mut terminal = InlineTerminal::new(backend, 4).expect("inline terminal");
        terminal.set_review_scroll_offset(8);
        let frame = VisualCanvasFrame {
            active_frame_lines: vec![
                CanvasLine::plain("history-one"),
                CanvasLine::plain("history-two"),
                CanvasLine::plain("streaming"),
                CanvasLine::plain("▌ prompt"),
                CanvasLine::plain("status"),
            ],
            cursor: None,
            required_height: 5,
            history_rows: 3,
            prefer_stable_height: false,
            committable_rows: 3,
            pinned_rows: 2,
        };

        terminal.draw_visual_frame(&frame).expect("draw frame");
        terminal.draw_visual_frame(&frame).expect("redraw frame");

        let rows = terminal.backend().scrollback_rows();
        assert_eq!(
            rows.iter()
                .filter(|row| row.contains("history-one"))
                .count(),
            1,
            "review-scrolled frame should not duplicate visible history through native commit: {rows:?}"
        );

        terminal.set_review_scroll_offset(0);
        terminal.draw_visual_frame(&frame).expect("draw tail frame");
        terminal
            .draw_visual_frame(&frame)
            .expect("redraw tail frame");

        let rows = terminal.backend().scrollback_rows();
        assert!(
            rows.iter().any(|row| row.contains("history-one")),
            "tail-hidden history should commit after review returns to tail: {rows:?}"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.contains("history-one"))
                .count(),
            1,
            "tail-hidden history should commit exactly once after returning to tail: {rows:?}"
        );
    }

    #[test]
    fn scrolled_history_commit_handles_multiple_rows_and_resize_reflow() {
        let backend = VT100Backend::new(30, 4);
        let mut terminal = InlineTerminal::new(backend, 3).expect("inline terminal");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("narrow-one-a"),
                    CanvasLine::plain("narrow-one-b"),
                    CanvasLine::plain("narrow-two-a"),
                    CanvasLine::plain("narrow-two-b"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 6,
                history_rows: 4,
                prefer_stable_height: false,
                committable_rows: 4,
                pinned_rows: 0,
            })
            .expect("draw narrow frame");

        terminal.backend_mut().resize(60, 4);
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("wide-one"),
                    CanvasLine::plain("wide-two"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 4,
                history_rows: 2,
                prefer_stable_height: false,
                committable_rows: 2,
                pinned_rows: 0,
            })
            .expect("draw wide frame");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("wide-one"),
                    CanvasLine::plain("wide-two"),
                    CanvasLine::plain("wide-three"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 5,
                history_rows: 3,
                prefer_stable_height: false,
                committable_rows: 3,
                pinned_rows: 0,
            })
            .expect("draw appended wide frame");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("wide-one"),
                    CanvasLine::plain("wide-two"),
                    CanvasLine::plain("wide-three"),
                    CanvasLine::plain("wide-four"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 6,
                history_rows: 4,
                prefer_stable_height: false,
                committable_rows: 4,
                pinned_rows: 0,
            })
            .expect("draw second appended wide frame");

        let rows = terminal.backend().scrollback_rows();
        assert_ordered_rows(&rows, &["narrow-one-a", "narrow-one-b", "narrow-two-a"]);
        assert_ordered_rows(&rows, &["wide-three", "wide-four", "▌ prompt", "status"]);
        assert!(
            rows.iter().all(|row| !row.contains("wide-one") && !row.contains("wide-two")),
            "old history should stay frozen in narrow scrollback, not be recommitted at wide width: {rows:?}"
        );
    }

    #[test]
    fn active_resize_preserves_committed_history_watermark() {
        let backend = VT100Backend::new(30, 4);
        let mut terminal = InlineTerminal::new(backend, 3).expect("inline terminal");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("history-one"),
                    CanvasLine::plain("history-two"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 4,
                history_rows: 2,
                prefer_stable_height: false,
                committable_rows: 2,
                pinned_rows: 0,
            })
            .expect("commit initial history");

        terminal.backend_mut().resize(60, 4);
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("history-one"),
                    CanvasLine::plain("history-two"),
                    CanvasLine::plain("live-tool"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 5,
                history_rows: 2,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw active resized frame");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("history-one"),
                    CanvasLine::plain("history-two"),
                    CanvasLine::plain("final-answer"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 5,
                history_rows: 3,
                prefer_stable_height: false,
                committable_rows: 3,
                pinned_rows: 0,
            })
            .expect("draw final resized frame");

        let rows = terminal.backend().scrollback_rows();
        assert_eq!(
            rows.iter()
                .filter(|row| row.contains("history-one"))
                .count(),
            1,
            "active resize forgot committed history and recommitted old rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("final-answer")),
            "newly hidden final history should still commit after active resize: {rows:?}"
        );
    }

    #[test]
    fn active_width_refreshes_resize_before_visual_frame_build() {
        let backend = VT100Backend::new(100, 10);
        let mut terminal = InlineTerminal::new(backend, 8).expect("inline terminal");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![CanvasLine::plain("▌"), CanvasLine::plain("status")],
                cursor: None,
                required_height: 2,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw frame");

        terminal.backend_mut().resize(72, 10);

        assert_eq!(
            terminal.observed_size_change().expect("observed resize"),
            Some(Size::new(72, 10))
        );
        assert_eq!(
            terminal
                .observed_size_change()
                .expect("duplicate observed resize suppressed"),
            None
        );
        assert_eq!(terminal.active_width().expect("active width"), 72);
        assert_eq!(terminal.viewport_area().width, 72);
        assert_eq!(terminal.observed_size_change().expect("no resize"), None);

        terminal.backend_mut().resize(64, 10);
        terminal.note_resize_event(64, 10);
        assert_eq!(
            terminal
                .observed_size_change()
                .expect("explicit resize event suppresses fallback"),
            None
        );
        assert_eq!(terminal.active_width().expect("explicit active width"), 64);
        assert_eq!(terminal.viewport_area().width, 64);
    }

    #[test]
    fn active_resize_with_reflowing_history_commits_new_final_tail() {
        let backend = VT100Backend::new(24, 3);
        let mut terminal = InlineTerminal::new(backend, 2).expect("inline terminal");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("narrow-one-a"),
                    CanvasLine::plain("narrow-one-b"),
                    CanvasLine::plain("narrow-two-a"),
                    CanvasLine::plain("narrow-two-b"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 6,
                history_rows: 4,
                prefer_stable_height: false,
                committable_rows: 4,
                pinned_rows: 0,
            })
            .expect("commit initial narrow history");

        terminal.backend_mut().resize(80, 3);
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("wide-history-one"),
                    CanvasLine::plain("wide-history-two"),
                    CanvasLine::plain("live-tool"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 5,
                history_rows: 2,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw active resized frame");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("wide-history-one"),
                    CanvasLine::plain("wide-history-two"),
                    CanvasLine::plain("final-answer-after-resize"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 5,
                history_rows: 3,
                prefer_stable_height: false,
                committable_rows: 3,
                pinned_rows: 0,
            })
            .expect("draw final resized frame");

        let rows = terminal.backend().scrollback_rows();
        assert_ordered_rows(&rows, &["narrow-one-b", "narrow-two-a", "narrow-two-b"]);
        assert!(
            rows.iter()
                .any(|row| row.contains("final-answer-after-resize")),
            "new final answer should commit after active resize: {rows:?}"
        );
        assert!(
            rows.iter()
                .all(|row| !row.contains("wide-history-one") && !row.contains("wide-history-two")),
            "reflowed old history should not be recommitted at new width: {rows:?}"
        );
    }

    #[test]
    fn bridge_resize_keeps_same_source_prefix_committed_once() {
        let backend = VT100Backend::new(40, 3);
        let mut terminal = InlineTerminal::new(backend, 3).expect("inline terminal");
        terminal.set_linefeed_history_insert_enabled(true);
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("same-prefix-one"),
                    CanvasLine::plain("same-prefix-two"),
                    CanvasLine::plain("same-prefix-three"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 5,
                history_rows: 3,
                prefer_stable_height: false,
                committable_rows: 3,
                pinned_rows: 0,
            })
            .expect("commit initial prefix");

        terminal.backend_mut().clear_raw_output();
        terminal.backend_mut().resize(80, 3);
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("same-prefix-one"),
                    CanvasLine::plain("same-prefix-two"),
                    CanvasLine::plain("same-prefix-three"),
                    CanvasLine::plain("same-prefix-four"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 6,
                history_rows: 1,
                prefer_stable_height: false,
                committable_rows: 4,
                pinned_rows: 0,
            })
            .expect("commit appended prefix after resize");

        let raw = terminal.backend().raw_output();
        assert!(
            !raw.windows("same-prefix-two".len())
                .any(|window| window == b"same-prefix-two"),
            "resize must not lower the source-prefix watermark and rewrite committed rows: {raw:?}"
        );
        assert!(
            raw.windows("same-prefix-three".len())
                .any(|window| window == b"same-prefix-three"),
            "newly hidden same-source rows should still commit after resize: {raw:?}"
        );
    }

    #[test]
    fn bridge_resize_suspension_clears_after_conservative_commit() {
        let backend = VT100Backend::new(40, 3);
        let mut terminal = InlineTerminal::new(backend, 3).expect("inline terminal");
        terminal.set_linefeed_history_insert_enabled(true);
        terminal.suspend_linefeed_history_insert_after_resize();
        terminal.backend_mut().clear_raw_output();
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("history-one"),
                    CanvasLine::plain("history-two"),
                    CanvasLine::plain("history-three"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 5,
                history_rows: 2,
                prefer_stable_height: false,
                committable_rows: 2,
                pinned_rows: 0,
            })
            .expect("commit with conservative path");

        let conservative_raw = terminal.backend().raw_output();
        assert!(
            !conservative_raw
                .windows(b"\x1b[1;3r\x1b[3;1H\r\n".len())
                .any(|window| window == b"\x1b[1;3r\x1b[3;1H\r\n"),
            "suspended bridge should not emit scroll-region insertion: {conservative_raw:?}"
        );

        terminal.backend_mut().clear_raw_output();
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("history-one"),
                    CanvasLine::plain("history-two"),
                    CanvasLine::plain("history-three"),
                    CanvasLine::plain("history-four"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 6,
                history_rows: 3,
                prefer_stable_height: false,
                committable_rows: 3,
                pinned_rows: 0,
            })
            .expect("commit next row after suspended bridge clears");

        let resumed_raw = terminal.backend().raw_output();
        assert!(
            resumed_raw
                .windows(b"\x1b[1;3r\x1b[3;1H\r\n".len())
                .any(|window| window == b"\x1b[1;3r\x1b[3;1H\r\n"),
            "bridge should resume after the conservative commit catches up: {resumed_raw:?}"
        );
    }

    #[test]
    fn bridge_resize_suspension_clears_after_one_conservative_flush() {
        let backend = VT100Backend::new(40, 3);
        let mut terminal = InlineTerminal::new(backend, 3).expect("inline terminal");
        terminal.set_linefeed_history_insert_enabled(true);
        terminal.suspend_linefeed_history_insert_after_resize();
        terminal.backend_mut().clear_raw_output();
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("history-one"),
                    CanvasLine::plain("history-two"),
                    CanvasLine::plain("history-three"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 5,
                history_rows: 2,
                prefer_stable_height: false,
                committable_rows: 4,
                pinned_rows: 0,
            })
            .expect("commit with conservative path");

        let conservative_raw = terminal.backend().raw_output();
        assert!(
            !conservative_raw
                .windows(b"\x1b[1;3r\x1b[3;1H\r\n".len())
                .any(|window| window == b"\x1b[1;3r\x1b[3;1H\r\n"),
            "suspended bridge should not emit scroll-region insertion: {conservative_raw:?}"
        );

        terminal.backend_mut().clear_raw_output();
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("history-one"),
                    CanvasLine::plain("history-two"),
                    CanvasLine::plain("history-three"),
                    CanvasLine::plain("history-four"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 6,
                history_rows: 3,
                prefer_stable_height: false,
                committable_rows: 4,
                pinned_rows: 0,
            })
            .expect("commit next row after one suspended bridge flush");

        let resumed_raw = terminal.backend().raw_output();
        assert!(
            resumed_raw
                .windows(b"\x1b[1;3r\x1b[3;1H\r\n".len())
                .any(|window| window == b"\x1b[1;3r\x1b[3;1H\r\n"),
            "bridge should resume after one conservative flush, even before committable catch-up: {resumed_raw:?}"
        );
    }

    #[test]
    fn bridge_resize_suspension_skips_blank_only_commit_without_fallback_clear() {
        let backend = VT100Backend::new(40, 3);
        let mut terminal = InlineTerminal::new(backend, 3).expect("inline terminal");
        terminal.set_linefeed_history_insert_enabled(true);
        terminal.suspend_linefeed_history_insert_after_resize();
        terminal.backend_mut().clear_raw_output();
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain(""),
                    CanvasLine::plain("history-three"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 4,
                history_rows: 1,
                prefer_stable_height: false,
                committable_rows: 1,
                pinned_rows: 0,
            })
            .expect("skip blank-only resize-suspended commit");

        let raw = terminal.backend().raw_output();
        assert!(
            !raw.windows(b"\x1b[1;3r\x1b[3;1H\r\n".len())
                .any(|window| window == b"\x1b[1;3r\x1b[3;1H\r\n"),
            "blank-only suspended commit should not use scroll-region insertion: {raw:?}"
        );
        assert!(
            !raw.windows(b"\x1b[1;1H\x1b[K\x1b[2;1H\x1b[K\x1b[3;1H\x1b[K".len())
                .any(|window| window == b"\x1b[1;1H\x1b[K\x1b[2;1H\x1b[K\x1b[3;1H\x1b[K"),
            "blank-only suspended commit should not clear the full active viewport before redraw: {raw:?}"
        );
        assert!(
            raw.windows("history-three".len())
                .any(|window| window == b"history-three"),
            "visible tail should still redraw after skipping blank-only commit: {raw:?}"
        );
    }

    #[test]
    fn visual_frame_height_changes_do_not_emit_erase_display_redraws() {
        let backend = VT100Backend::new(30, 6);
        let mut terminal = InlineTerminal::new(backend, 6).expect("inline terminal");
        terminal.backend_mut().clear_raw_output();

        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![CanvasLine::plain("one"), CanvasLine::plain("status")],
                cursor: None,
                required_height: 2,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw compact frame");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("one"),
                    CanvasLine::plain("two"),
                    CanvasLine::plain("three"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 4,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw grown frame");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![CanvasLine::plain("done"), CanvasLine::plain("status")],
                cursor: None,
                required_height: 2,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw compact frame again");

        let raw = terminal.backend().raw_output();
        assert!(
            !raw.windows(3).any(|window| window == b"\x1b[J"),
            "visual frame draw should use line clears, not erase-display redraws: {raw:?}"
        );
        assert!(
            raw.windows(3).any(|window| window == b"\x1b[K"),
            "visual frame draw should still clear stale line tails: {raw:?}"
        );
        let first_clear = find_bytes(raw, b"\x1b[K").expect("line clear should be queued");
        let first_text = find_bytes(raw, b"one").expect("frame text should be queued");
        assert!(
            first_clear < first_text,
            "visual frame should clear a row before writing into it to avoid right-edge autowrap clears: {raw:?}"
        );
    }

    #[test]
    fn compacting_live_frame_writes_new_content_before_stale_row_clears() {
        let backend = VT100Backend::new(30, 6);
        let mut terminal = InlineTerminal::new(backend, 6).expect("inline terminal");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("old one"),
                    CanvasLine::plain("old two"),
                    CanvasLine::plain("old three"),
                    CanvasLine::plain("old status"),
                ],
                cursor: None,
                required_height: 4,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw tall frame");

        terminal.backend_mut().clear_raw_output();
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("new prompt"),
                    CanvasLine::plain("new status"),
                ],
                cursor: None,
                required_height: 2,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw compact frame");

        let raw = terminal.backend().raw_output();
        let first_text = find_bytes(raw, b"new prompt").expect("new frame text should be queued");
        let clears_before_first_text = count_bytes_before(raw, b"\x1b[K", first_text);
        assert_eq!(
            clears_before_first_text, 1,
            "compacting redraw should clear only the target row before first new content, not blank the old frame first: {raw:?}"
        );
    }

    #[test]
    fn stable_viewport_redraw_only_writes_changed_rows() {
        let backend = VT100Backend::new(50, 8);
        let mut terminal = InlineTerminal::new(backend, 8).expect("inline terminal");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("stable transcript one"),
                    CanvasLine::plain("stable transcript two"),
                    CanvasLine::plain("stable transient"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 5,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw initial frame");

        terminal.backend_mut().clear_raw_output();
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("stable transcript one"),
                    CanvasLine::plain("stable transcript two"),
                    CanvasLine::plain("changed transient notice"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 5,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw changed transient frame");

        let raw = terminal.backend().raw_output();
        assert!(
            raw.windows("changed transient notice".len())
                .any(|window| window == b"changed transient notice"),
            "changed row should be written: {raw:?}"
        );
        assert!(
            !raw.windows("stable transcript one".len())
                .any(|window| window == b"stable transcript one"),
            "unchanged top row should not be rewritten: {raw:?}"
        );
        assert!(
            !raw.windows("stable transcript two".len())
                .any(|window| window == b"stable transcript two"),
            "unchanged second row should not be rewritten: {raw:?}"
        );
    }

    #[test]
    fn scrolled_history_commit_retries_after_write_error() {
        let backend = VT100Backend::new(30, 4);
        let mut terminal = InlineTerminal::new(backend, 3).expect("inline terminal");
        let frame = VisualCanvasFrame {
            active_frame_lines: vec![
                CanvasLine::plain("history-one"),
                CanvasLine::plain("history-two"),
                CanvasLine::plain("▌ prompt"),
                CanvasLine::plain("status"),
            ],
            cursor: None,
            required_height: 4,
            history_rows: 2,
            prefer_stable_height: false,
            committable_rows: 2,
            pinned_rows: 0,
        };

        terminal.backend_mut().set_write_error(true);
        terminal
            .draw_visual_frame(&frame)
            .expect_err("forced write error should fail history commit");
        terminal.backend_mut().set_write_error(false);
        terminal
            .draw_visual_frame(&frame)
            .expect("retry draw frame");

        let rows = terminal.backend().scrollback_rows();
        assert_ordered_rows(&rows, &["history-one", "history-two", "▌ prompt", "status"]);
    }

    #[test]
    fn appending_rows_after_stable_footer_does_not_rewrite_footer_prefix() {
        let backend = VT100Backend::new(40, 4);
        let mut terminal = InlineTerminal::new(backend, 4).expect("inline terminal");
        terminal
            .write_finalized_lines(&[
                CanvasLine::plain("history-one"),
                CanvasLine::plain("history-two"),
                CanvasLine::plain("history-three"),
            ])
            .expect("write finalized");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("▌ "),
                    CanvasLine::plain("fixture/echo status"),
                ],
                cursor: None,
                required_height: 2,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw compact footer");
        terminal.backend_mut().clear_raw_output();

        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("▌ "),
                    CanvasLine::plain("fixture/echo status"),
                    CanvasLine::plain(" /"),
                    CanvasLine::plain("> /model switch provider/model"),
                    CanvasLine::plain("(1/9)"),
                    CanvasLine::plain(" Enter select  Tab complete  Esc close"),
                ],
                cursor: None,
                required_height: 6,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw appended slash surface");

        let raw = terminal.backend().raw_output();
        assert!(
            !raw.windows("fixture/echo status".len())
                .any(|window| window == b"fixture/echo status"),
            "stable status row should not be rewritten: {raw:?}"
        );
        assert!(
            raw.windows("/model switch".len())
                .any(|window| window == b"/model switch"),
            "slash rows should be written: {raw:?}"
        );
    }

    #[test]
    fn finalized_lines_use_scroll_region_above_bottom_band_when_geometry_is_safe() {
        let backend = VT100Backend::new(32, 6);
        let mut terminal = InlineTerminal::new(backend, 2).expect("inline terminal");
        terminal.set_linefeed_history_insert_enabled(true);
        terminal
            .set_viewport_area(Rect::new(0, 4, 32, 2))
            .expect("set bottom-band viewport");
        write!(
            terminal.backend_mut(),
            "\x1b[1;1Hold-one\x1b[2;1Hold-two\x1b[3;1Hold-three\x1b[4;1Hold-four\
             \x1b[5;1H▌ prompt\x1b[6;1Hstatus"
        )
        .expect("seed terminal");
        terminal.backend_mut().clear_raw_output();

        terminal
            .write_finalized_lines(&[CanvasLine::plain("inserted history")])
            .expect("insert finalized history");

        let raw = terminal.backend().raw_output();
        assert!(
            raw.starts_with(b"\x1b[1;4r\x1b[4;1H\r\n"),
            "history insert should use scroll region + linefeed before writing text: {raw:?}"
        );
        assert!(
            raw.windows("inserted history".len())
                .any(|window| window == b"inserted history"),
            "history insert should write the finalized line after linefeed insertion: {raw:?}"
        );
        assert!(
            raw.windows(3).any(|window| window == b"\x1b[r"),
            "history insert should reset the scroll region: {raw:?}"
        );
        assert!(!raw.windows(3).any(|window| window == b"\x1b[S"));
        assert!(!raw.windows(3).any(|window| window == b"\x1b[J"));

        let rows = trimmed_rows(terminal.backend());
        assert_eq!(rows[2], "old-four");
        assert_eq!(rows[3], "inserted history");
        assert_eq!(rows[4], "▌ prompt");
        assert_eq!(rows[5], "status");
    }

    #[test]
    fn finalized_lines_keep_legacy_active_viewport_write_by_default() {
        let backend = VT100Backend::new(32, 6);
        let mut terminal = InlineTerminal::new(backend, 2).expect("inline terminal");
        terminal
            .set_viewport_area(Rect::new(0, 4, 32, 2))
            .expect("set bottom-band viewport");
        write!(
            terminal.backend_mut(),
            "\x1b[1;1Hold-one\x1b[2;1Hold-two\x1b[3;1Hold-three\x1b[4;1Hold-four\
             \x1b[5;1H▌ prompt\x1b[6;1Hstatus"
        )
        .expect("seed terminal");
        terminal.backend_mut().clear_raw_output();

        terminal
            .write_finalized_lines(&[CanvasLine::plain("inserted history")])
            .expect("write finalized history");

        let raw = terminal.backend().raw_output();
        assert!(
            !raw.windows(b"\x1b[1;4r".len())
                .any(|window| window == b"\x1b[1;4r"),
            "default finalized write should not use the experimental scroll-region bridge: {raw:?}"
        );
        assert!(
            raw.windows("inserted history".len())
                .any(|window| window == b"inserted history"),
            "default finalized write should still render finalized text: {raw:?}"
        );
    }

    #[test]
    fn finalized_lines_bridge_inserts_wrapped_rows_without_touching_bottom_band() {
        let backend = VT100Backend::new(8, 8);
        let mut terminal = InlineTerminal::new(backend, 2).expect("inline terminal");
        terminal.set_linefeed_history_insert_enabled(true);
        terminal
            .set_viewport_area(Rect::new(0, 6, 8, 2))
            .expect("set bottom-band viewport");
        write!(
            terminal.backend_mut(),
            "\x1b[1;1Hold-one\x1b[2;1Hold-two\x1b[3;1Hold-three\x1b[4;1Hold-four\
             \x1b[5;1Hold-five\x1b[6;1Hold-six\x1b[7;1H▌ prompt\x1b[8;1Hstatus"
        )
        .expect("seed terminal");
        terminal.backend_mut().clear_raw_output();

        terminal
            .write_finalized_lines(&[CanvasLine::plain("abcdefghi")])
            .expect("insert wrapped history");

        let raw = terminal.backend().raw_output();
        assert!(
            raw.starts_with(b"\x1b[1;6r\x1b[6;1H\r\n"),
            "wrapped bridge should use the history region above the bottom band: {raw:?}"
        );
        assert_eq!(
            raw.windows(b"\r\n".len())
                .filter(|window| *window == b"\r\n")
                .count(),
            2
        );

        let rows = trimmed_rows(terminal.backend());
        assert_eq!(rows[4], "abcdefgh");
        assert_eq!(rows[5], "i");
        assert_eq!(rows[6], "▌ prompt");
        assert_eq!(rows[7], "status");
    }

    #[test]
    fn history_commit_bridge_uses_visible_pinned_footer_rows() {
        let backend = VT100Backend::new(40, 6);
        let mut terminal = InlineTerminal::new(backend, 6).expect("inline terminal");
        terminal.set_linefeed_history_insert_enabled(true);
        terminal.backend_mut().clear_raw_output();

        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("history-one"),
                    CanvasLine::plain("history-two"),
                    CanvasLine::plain("history-three"),
                    CanvasLine::plain("history-four"),
                    CanvasLine::plain("history-five"),
                    CanvasLine::plain("history-six"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 8,
                history_rows: 6,
                prefer_stable_height: false,
                committable_rows: 6,
                pinned_rows: 2,
            })
            .expect("draw with history commit bridge");

        let raw = terminal.backend().raw_output();
        assert!(
            raw.windows(b"\x1b[1;4r\x1b[4;1H\r\n".len())
                .any(|window| window == b"\x1b[1;4r\x1b[4;1H\r\n"),
            "history commit should preserve the pinned footer as the bottom band: {raw:?}"
        );
        assert!(
            raw.windows("history-one".len())
                .any(|window| window == b"history-one"),
            "history commit should write hidden history rows: {raw:?}"
        );
        assert!(
            raw.windows(3).any(|window| window == b"\x1b[r"),
            "history commit bridge should reset the scroll region: {raw:?}"
        );
        assert!(!raw.windows(3).any(|window| window == b"\x1b[S"));
        assert!(!raw.windows(3).any(|window| window == b"\x1b[J"));

        let rows = trimmed_rows(terminal.backend());
        assert_eq!(rows[4], "▌ prompt");
        assert_eq!(rows[5], "status");
    }

    #[test]
    fn history_commit_bridge_is_default_off_for_same_footer_geometry() {
        let backend = VT100Backend::new(40, 6);
        let mut terminal = InlineTerminal::new(backend, 6).expect("inline terminal");
        terminal.backend_mut().clear_raw_output();

        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("history-one"),
                    CanvasLine::plain("history-two"),
                    CanvasLine::plain("history-three"),
                    CanvasLine::plain("history-four"),
                    CanvasLine::plain("history-five"),
                    CanvasLine::plain("history-six"),
                    CanvasLine::plain("▌ prompt"),
                    CanvasLine::plain("status"),
                ],
                cursor: None,
                required_height: 8,
                history_rows: 6,
                prefer_stable_height: false,
                committable_rows: 6,
                pinned_rows: 2,
            })
            .expect("draw with default history commit");

        let raw = terminal.backend().raw_output();
        assert!(
            !raw.windows(b"\x1b[1;4r\x1b[4;1H\r\n".len())
                .any(|window| window == b"\x1b[1;4r\x1b[4;1H\r\n"),
            "default-off history commit should not use the experimental bridge: {raw:?}"
        );
        assert!(
            raw.windows("history-one".len())
                .any(|window| window == b"history-one"),
            "default history commit should still render hidden history rows: {raw:?}"
        );
    }

    #[test]
    fn history_commit_bridge_counts_wrapped_pinned_footer_rows() {
        let backend = VT100Backend::new(10, 6);
        let mut terminal = InlineTerminal::new(backend, 6).expect("inline terminal");
        terminal.set_linefeed_history_insert_enabled(true);
        terminal.backend_mut().clear_raw_output();

        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("h1"),
                    CanvasLine::plain("h2"),
                    CanvasLine::plain("h3"),
                    CanvasLine::plain("h4"),
                    CanvasLine::plain("h5"),
                    CanvasLine::plain("h6"),
                    CanvasLine::plain("h7"),
                    CanvasLine::plain("status-wraps"),
                ],
                cursor: None,
                required_height: 8,
                history_rows: 7,
                prefer_stable_height: false,
                committable_rows: 7,
                pinned_rows: 1,
            })
            .expect("draw with wrapped footer");

        let raw = terminal.backend().raw_output();
        assert!(
            raw.windows(b"\x1b[1;4r\x1b[4;1H\r\n".len())
                .any(|window| window == b"\x1b[1;4r\x1b[4;1H\r\n"),
            "wrapped pinned footer should reserve two rendered rows: {raw:?}"
        );
    }

    #[test]
    fn history_commit_bridge_allows_zero_pinned_rows() {
        let backend = VT100Backend::new(40, 6);
        let mut terminal = InlineTerminal::new(backend, 4).expect("inline terminal");
        terminal.set_linefeed_history_insert_enabled(true);
        terminal.backend_mut().clear_raw_output();

        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("history-one"),
                    CanvasLine::plain("history-two"),
                    CanvasLine::plain("history-three"),
                    CanvasLine::plain("history-four"),
                    CanvasLine::plain("history-five"),
                    CanvasLine::plain("history-six"),
                ],
                cursor: None,
                required_height: 6,
                history_rows: 6,
                prefer_stable_height: false,
                committable_rows: 6,
                pinned_rows: 0,
            })
            .expect("draw with no pinned footer");

        let raw = terminal.backend().raw_output();
        assert!(
            raw.windows(b"\x1b[1;6r\x1b[6;1H\r\n".len())
                .any(|window| window == b"\x1b[1;6r\x1b[6;1H\r\n"),
            "zero pinned rows should use the full screen as the history region: {raw:?}"
        );
    }

    #[test]
    fn finalized_lines_bridge_falls_back_when_no_safe_history_region_exists() {
        let backend = VT100Backend::new(16, 4);
        let mut terminal = InlineTerminal::new(backend, 4).expect("inline terminal");
        terminal.set_linefeed_history_insert_enabled(true);
        terminal
            .set_viewport_area(Rect::new(0, 0, 16, 4))
            .expect("set full-screen viewport");
        terminal.backend_mut().clear_raw_output();

        terminal
            .write_finalized_lines(&[CanvasLine::plain("fallback")])
            .expect("write fallback history");

        let raw = terminal.backend().raw_output();
        assert!(
            !raw.windows(b"\x1b[r".len())
                .any(|window| window == b"\x1b[r"),
            "degenerate geometry should not enter the bridge path: {raw:?}"
        );
        assert!(
            raw.windows("fallback".len())
                .any(|window| window == b"fallback"),
            "legacy fallback should still render finalized text: {raw:?}"
        );
    }

    #[test]
    fn stable_height_frame_crops_top_rows_instead_of_scrolling_terminal() {
        let backend = VT100Backend::new(50, 6);
        let mut terminal = InlineTerminal::new(backend, 6).expect("inline terminal");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("transcript one"),
                    CanvasLine::plain("transcript two"),
                    CanvasLine::plain("transcript three"),
                    CanvasLine::plain("transcript four"),
                    CanvasLine::plain("▌ "),
                    CanvasLine::plain("fixture/echo status"),
                ],
                cursor: None,
                required_height: 6,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw full live frame");
        let before = terminal.viewport_area();
        terminal.backend_mut().clear_raw_output();

        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("transcript one"),
                    CanvasLine::plain("transcript two"),
                    CanvasLine::plain("transcript three"),
                    CanvasLine::plain("transcript four"),
                    CanvasLine::plain("▌ "),
                    CanvasLine::plain("fixture/echo status"),
                    CanvasLine::plain(" /"),
                    CanvasLine::plain("> /model switch provider/model"),
                    CanvasLine::plain("(1/9)"),
                    CanvasLine::plain(" Enter select  Tab complete  Esc close"),
                ],
                cursor: None,
                required_height: 10,
                history_rows: 0,
                prefer_stable_height: true,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw stable-height slash frame");

        assert_eq!(terminal.viewport_area(), before);
        let rows = trimmed_rows(terminal.backend());
        assert!(
            !rows.iter().any(|row| row.contains("transcript one")),
            "rows: {rows:?}"
        );
        assert_ordered_rows(
            &rows,
            &[
                "▌",
                "fixture/echo status",
                " /",
                "/model switch provider/model",
                "(1/9)",
            ],
        );
        let raw = terminal.backend().raw_output();
        assert!(
            !raw.windows("transcript one".len())
                .any(|window| window == b"transcript one"),
            "stable-height slash draw should not scroll hidden transcript rows into native output: {raw:?}"
        );
    }

    #[test]
    fn growing_active_frame_near_bottom_does_not_leave_old_chrome() {
        let backend = VT100Backend::new(20, 4);
        let mut terminal = InlineTerminal::new(backend, 4).expect("inline terminal");
        terminal
            .write_finalized_lines(&[
                CanvasLine::plain("final-one"),
                CanvasLine::plain("final-two"),
                CanvasLine::plain("final-three"),
            ])
            .expect("write finalized");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![CanvasLine::plain("stale-status")],
                cursor: None,
                required_height: 1,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw old active");

        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![CanvasLine::plain("draft"), CanvasLine::plain("status")],
                cursor: None,
                required_height: 2,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw grown active");

        let all_rows = terminal.backend().scrollback_rows().join("\n");
        assert!(!all_rows.contains("stale-status"), "rows: {all_rows:?}");
        assert!(all_rows.contains("draft"), "rows: {all_rows:?}");
        assert!(all_rows.contains("status"), "rows: {all_rows:?}");
    }

    #[test]
    fn resize_from_tall_active_frame_to_compact_prompt_leaves_no_blank_canyon() {
        let backend = VT100Backend::new(30, 12);
        let mut terminal = InlineTerminal::new(backend, 12).expect("inline terminal");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("live transcript"),
                    CanvasLine::plain("more live transcript"),
                    CanvasLine::plain("▌ draft one"),
                    CanvasLine::plain("▌ draft two"),
                    CanvasLine::plain("◦ Working (9s • esc to interrupt)"),
                ],
                cursor: None,
                required_height: 5,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw tall frame");

        terminal.backend_mut().resize(24, 6);
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("▌ "),
                    CanvasLine::plain("fixture/echo ? · Context ?% used"),
                ],
                cursor: None,
                required_height: 2,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw compact frame");

        let rows = trimmed_rows(terminal.backend());
        let prompt_rows = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.starts_with("▌"))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert_eq!(prompt_rows.len(), 1, "rows: {rows:?}");
        let status = row_containing(&rows, "fixture/echo");
        assert_eq!(status, prompt_rows[0] + 1, "rows: {rows:?}");
        assert!(
            !rows.iter().any(|row| row.contains("live transcript")),
            "rows: {rows:?}"
        );
    }

    #[test]
    fn over_wide_row_on_bottom_screen_row_never_scrolls_the_screen() {
        // Regression: after a terminal resize narrows the screen, live rows
        // can be wider than the new width. Printing one unclipped on the
        // bottom screen row auto-wraps, physically scrolls the terminal, and
        // pushes the rows above (banner, transcript) off-screen.
        let backend = VT100Backend::new(24, 6);
        let mut terminal = InlineTerminal::new(backend, 6).expect("inline terminal");
        terminal
            .draw_visual_frame(&VisualCanvasFrame {
                active_frame_lines: vec![
                    CanvasLine::plain("EULER-TITLE"),
                    CanvasLine::plain("transcript-one"),
                    CanvasLine::plain("transcript-two"),
                    CanvasLine::plain("transcript-three"),
                    CanvasLine::plain("▌ draft"),
                    CanvasLine::plain("status line far wider than twenty-four columns"),
                ],
                cursor: None,
                required_height: 6,
                history_rows: 0,
                prefer_stable_height: false,
                committable_rows: 0,
                pinned_rows: 0,
            })
            .expect("draw full-height frame with over-wide bottom row");

        let rows = trimmed_rows(terminal.backend());
        assert_eq!(
            rows[0], "EULER-TITLE",
            "top row must survive drawing an over-wide bottom row: {rows:?}"
        );
        assert!(
            rows[5].starts_with("status line"),
            "bottom row shows the clipped status line: {rows:?}"
        );
    }

    fn trimmed_rows(backend: &VT100Backend) -> Vec<String> {
        backend
            .screen_rows()
            .into_iter()
            .map(|row| row.trim_end().to_owned())
            .collect()
    }

    fn row_containing(rows: &[String], needle: &str) -> usize {
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("expected row containing {needle:?}, rows: {rows:?}"))
    }

    fn assert_ordered_rows(rows: &[String], needles: &[&str]) {
        let mut start = 0;
        for needle in needles {
            let Some(relative) = rows.iter().skip(start).position(|row| row.contains(needle))
            else {
                panic!("expected ordered row containing {needle:?}, rows: {rows:?}");
            };
            start += relative + 1;
        }
    }
}
