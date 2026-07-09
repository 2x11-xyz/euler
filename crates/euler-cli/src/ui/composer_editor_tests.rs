use super::*;

#[test]
fn cursor_insert_delete_and_backspace_work_at_boundaries() {
    let mut draft = ComposerDraft::new();
    draft.insert_text("abc");
    draft.move_left();
    draft.insert_text("X");
    assert_eq!(draft.submit_text(), "abXc");

    draft.move_home();
    draft.backspace();
    assert_eq!(draft.submit_text(), "abXc");
    draft.delete();
    assert_eq!(draft.submit_text(), "bXc");

    draft.move_end();
    draft.delete();
    assert_eq!(draft.submit_text(), "bXc");
    draft.backspace();
    assert_eq!(draft.submit_text(), "bX");
    assert_eq!(draft.cursor_offset(), 2);
}

#[test]
fn cursor_left_right_home_and_end_track_one_canonical_offset() {
    let mut draft = ComposerDraft::new();
    draft.insert_text("ab\ncd");

    assert_eq!(draft.cursor_offset(), 5);
    draft.move_home();
    assert_eq!(draft.cursor_offset(), 3);
    draft.move_left();
    assert_eq!(draft.cursor_offset(), 2);
    draft.move_right();
    assert_eq!(draft.cursor_offset(), 3);
    draft.move_end();
    assert_eq!(draft.cursor_offset(), 5);
}

#[test]
fn newline_boundary_belongs_to_previous_logical_line() {
    let mut draft = ComposerDraft::new();
    draft.insert_text("ab\ncd");
    draft.move_home();
    draft.move_left();

    draft.move_home();
    assert_eq!(draft.cursor_offset(), 0);
    draft.move_end();
    assert_eq!(draft.cursor_offset(), 2);
}

#[test]
fn insert_char_and_insert_newline_store_canonical_lf() {
    let mut draft = ComposerDraft::new();
    draft.insert_text("a");
    draft.insert_char('\r');
    draft.insert_newline();
    draft.insert_text("b");

    assert_eq!(draft.submit_text(), "a\n\nb");
}

#[test]
fn explicit_line_up_down_retains_preferred_column() {
    let mut draft = ComposerDraft::new();
    draft.insert_text("abcd\nx\n123456");

    draft.move_up();
    assert_eq!(draft.cursor_offset(), 6);
    draft.move_up();
    assert_eq!(draft.cursor_offset(), 4);
    draft.move_down();
    assert_eq!(draft.cursor_offset(), 6);
    draft.move_down();
    assert_eq!(draft.cursor_offset(), 13);
}

#[test]
fn explicit_line_up_down_uses_current_cursor_column_not_line_end() {
    let mut draft = ComposerDraft::new();
    draft.insert_text("abcd\n1234");
    draft.move_home();
    draft.move_right();
    draft.move_right();

    draft.move_up();

    assert_eq!(draft.cursor_offset(), 2);
}

#[test]
fn paste_tokens_are_atomic_for_cursor_movement_and_deletion() {
    let payload = "p".repeat(1_001);
    let mut draft = ComposerDraft::new();
    draft.insert_text("a");
    draft.insert_bracketed_paste(&payload);
    draft.insert_text("b");

    assert_eq!(draft.cursor_offset(), 3);
    draft.move_left();
    assert_eq!(draft.cursor_offset(), 2);
    draft.move_left();
    assert_eq!(draft.cursor_offset(), 1);
    draft.move_right();
    assert_eq!(draft.cursor_offset(), 2);
    draft.backspace();
    assert_eq!(draft.render_text(), "ab");
    assert_eq!(draft.submit_text(), "ab");
    assert_eq!(draft.cursor_offset(), 1);

    let mut delete_draft = ComposerDraft::new();
    delete_draft.insert_text("a");
    delete_draft.insert_bracketed_paste(&payload);
    delete_draft.insert_text("b");
    delete_draft.move_home();
    delete_draft.move_right();
    delete_draft.delete();
    assert_eq!(delete_draft.render_text(), "ab");
}

#[test]
fn vertical_navigation_lands_on_paste_token_boundaries_atomically() {
    let payload = "p".repeat(1_001);
    let mut draft = ComposerDraft::new();
    draft.insert_text("abc\n");
    draft.insert_text("a");
    draft.insert_bracketed_paste(&payload);
    draft.insert_text("z");
    draft.move_home();
    draft.move_up();
    draft.move_right();
    draft.move_right();

    draft.move_down();
    assert_eq!(draft.cursor_offset(), 5);
    draft.move_right();
    assert_eq!(draft.cursor_offset(), 6);
}

#[test]
fn mid_buffer_edits_submit_hidden_paste_payload_once() {
    let payload = "p".repeat(1_001);
    let mut draft = ComposerDraft::new();
    draft.insert_text("left ");
    draft.insert_bracketed_paste(&payload);
    draft.insert_text(" [paste #1 1001 chars]");
    draft.move_home();
    draft.insert_text("start ");
    draft.move_end();
    draft.backspace();

    assert_eq!(draft.submit_text().matches(&payload).count(), 1);
    assert!(draft.submit_text().contains("[paste #1 1001 chars"));
    assert!(draft.render_text().contains("[paste #1 1001 chars]"));
}

#[test]
fn non_ascii_cursor_policy_uses_display_width_boundaries() {
    let mut draft = ComposerDraft::new();
    draft.insert_text("a\u{754c}b");
    draft.move_home();
    draft.move_right();
    assert_eq!(
        cursor_position(&draft, 20, &ComposerRenderOptions::default(), 3).column,
        3
    );
    draft.move_right();
    assert_eq!(
        cursor_position(&draft, 20, &ComposerRenderOptions::default(), 3).column,
        5
    );
}

#[test]
fn cursor_position_respects_scroll_window() {
    let mut draft = ComposerDraft::new();
    draft.insert_text("line1\nline2\nline3\nabcdef");
    draft.move_up();
    draft.set_scroll_line(1);
    let options = ComposerRenderOptions {
        max_visible_lines: 2,
    };

    let position = cursor_position(&draft, 20, &options, 4);

    assert_eq!(position.logical_line, 2);
    assert_eq!(position.visible_row, Some(1));
    assert_eq!(position.column, 7);
    assert!(matches!(
        render_lines(
            &ComposerSnapshot::new(&draft),
            &options,
            20,
            2
        )
        .as_slice(),
        [
            ComposerLine::Draft { text: line2, indicator: Some(OverflowIndicator::Above), .. },
            ComposerLine::Draft { text: line3, indicator: Some(OverflowIndicator::Below), .. },
        ] if line2 == "line2" && line3 == "line3"
    ));
}

#[test]
fn manual_multiline_composer_keeps_cursor_visible_after_visible_cap() {
    let mut draft = ComposerDraft::new();
    draft.insert_text("line1\nline2\nline3\nline4\nline5\nline6");
    let snapshot = ComposerSnapshot::new(&draft);
    let options = ComposerRenderOptions::default();

    let rows = render_lines(&snapshot, &options, 80, 5);
    let position = cursor_position(&draft, 80, &options, 5);

    assert_eq!(position.visible_row, Some(4));
    assert!(matches!(
        rows.as_slice(),
        [
            ComposerLine::Draft {
                indicator: Some(OverflowIndicator::Above),
                text: line2,
                ..
            },
            ComposerLine::Draft { text: line3, .. },
            ComposerLine::Draft { text: line4, .. },
            ComposerLine::Draft { text: line5, .. },
            ComposerLine::Draft { text: line6, .. },
        ] if line2 == "line2"
            && line3 == "line3"
            && line4 == "line4"
            && line5 == "line5"
            && line6 == "line6"
    ));
}

#[test]
fn composer_cursor_visible_with_long_wrapped_logical_line() {
    let mut draft = ComposerDraft::new();
    draft.insert_text("abcdefghijklmnopqrstuvwxyz");
    let snapshot = ComposerSnapshot::new(&draft);
    let options = ComposerRenderOptions::default();

    let rows = render_lines(&snapshot, &options, 7, 5);
    let position = cursor_position(&draft, 7, &options, 5);

    assert_eq!(position.visible_row, Some(4));
    assert!(matches!(
        rows.as_slice(),
        [
            ComposerLine::Draft {
                indicator: Some(OverflowIndicator::Above),
                text: fghij,
                ..
            },
            ComposerLine::Draft { text: klmno, .. },
            ComposerLine::Draft { text: pqrst, .. },
            ComposerLine::Draft { text: uvwxy, .. },
            ComposerLine::Draft { text: z, .. },
        ] if fghij == "fghij"
            && klmno == "klmno"
            && pqrst == "pqrst"
            && uvwxy == "uvwxy"
            && z == "z"
    ));
}

#[test]
fn composer_cursor_visible_on_blank_row_after_visible_cap() {
    let mut draft = ComposerDraft::new();
    draft.insert_text("line1\nline2\nline3\nline4\nline5\n");
    let snapshot = ComposerSnapshot::new(&draft);
    let options = ComposerRenderOptions::default();

    let rows = render_lines(&snapshot, &options, 80, 5);
    let position = cursor_position(&draft, 80, &options, 5);

    assert_eq!(position.visible_row, Some(4));
    assert!(matches!(
    rows.as_slice(),
    [
        ComposerLine::Draft {
            indicator: Some(OverflowIndicator::Above),
            text: line2,
            ..
        },
        ComposerLine::Draft { text: line3, .. },
        ComposerLine::Draft { text: line4, .. },
        ComposerLine::Draft { text: line5, .. },
        ComposerLine::Draft { text: blank, .. },
    ] if line2 == "line2"
        && line3 == "line3"
        && line4 == "line4"
        && line5 == "line5"
        && blank.is_empty()
    ));
}

#[test]
fn composer_preserves_requested_scroll_when_cursor_inside_window() {
    let mut draft = ComposerDraft::new();
    draft.insert_text("line1\nline2\nline3\nline4\nline5\nline6");
    draft.cursor = "line1\nline2\n".chars().count();
    draft.set_scroll_line(1);
    let snapshot = ComposerSnapshot::new(&draft);
    let options = ComposerRenderOptions::default();

    let rows = render_lines(&snapshot, &options, 80, 5);
    let position = cursor_position(&draft, 80, &options, 5);

    assert_eq!(position.visible_row, Some(1));
    assert!(matches!(
    rows.as_slice(),
    [
        ComposerLine::Draft {
            indicator: Some(OverflowIndicator::Above),
            text: line2,
            ..
        },
        ComposerLine::Draft { text: line3, .. },
        ComposerLine::Draft { text: line4, .. },
        ComposerLine::Draft { text: line5, .. },
        ComposerLine::Draft { text: line6, .. },
    ] if line2 == "line2"
        && line3 == "line3"
        && line4 == "line4"
        && line5 == "line5"
        && line6 == "line6"
    ));
}

#[test]
fn composer_preserves_requested_scroll_on_visible_window_boundaries() {
    let mut first = ComposerDraft::new();
    first.insert_text("line1\nline2\nline3\nline4\nline5\nline6");
    first.cursor = "line1\n".chars().count();
    first.set_scroll_line(1);

    let mut last = ComposerDraft::new();
    last.insert_text("line1\nline2\nline3\nline4\nline5\nline6");
    last.set_scroll_line(1);

    let options = ComposerRenderOptions::default();
    let first_rows = render_lines(&ComposerSnapshot::new(&first), &options, 80, 5);
    let last_rows = render_lines(&ComposerSnapshot::new(&last), &options, 80, 5);

    assert_eq!(
        cursor_position(&first, 80, &options, 5).visible_row,
        Some(0)
    );
    assert_eq!(cursor_position(&last, 80, &options, 5).visible_row, Some(4));
    assert!(matches!(
        first_rows.as_slice(),
        [
            ComposerLine::Draft { text: line2, .. },
            ComposerLine::Draft { text: line3, .. },
            ComposerLine::Draft { text: line4, .. },
            ComposerLine::Draft { text: line5, .. },
            ComposerLine::Draft { text: line6, .. },
        ] if line2 == "line2"
            && line3 == "line3"
            && line4 == "line4"
            && line5 == "line5"
            && line6 == "line6"
    ));
    assert_eq!(first_rows, last_rows);
}

#[test]
fn composer_empty_draft_keeps_single_visible_cursor_row() {
    let draft = ComposerDraft::new();
    let snapshot = ComposerSnapshot::new(&draft);
    let options = ComposerRenderOptions::default();

    let rows = render_lines(&snapshot, &options, 80, 5);
    let position = cursor_position(&draft, 80, &options, 5);

    assert_eq!(position.visible_row, Some(0));
    assert!(matches!(
        rows.as_slice(),
        [ComposerLine::Draft {
            indicator: None,
            prompt: true,
            text,
            ghost: true,
            ..
        }] if text == "message euler · / commands"
    ));
}

#[test]
fn composer_resize_recomputes_wrapped_cursor_window() {
    let mut draft = ComposerDraft::new();
    draft.insert_text("abcdefghijklmnopqrstuvwxyz");
    let snapshot = ComposerSnapshot::new(&draft);
    let options = ComposerRenderOptions::default();

    let wide_rows = render_lines(&snapshot, &options, 80, 5);
    let wide_position = cursor_position(&draft, 80, &options, 5);
    let narrow_rows = render_lines(&snapshot, &options, 7, 5);
    let narrow_position = cursor_position(&draft, 7, &options, 5);

    assert_eq!(wide_position.visible_row, Some(0));
    assert!(matches!(
        wide_rows.as_slice(),
        [ComposerLine::Draft { text, .. }] if text == "abcdefghijklmnopqrstuvwxyz"
    ));
    assert_eq!(narrow_position.visible_row, Some(4));
    assert!(matches!(
        narrow_rows.as_slice(),
        [
            ComposerLine::Draft { text: fghij, .. },
            ComposerLine::Draft { text: klmno, .. },
            ComposerLine::Draft { text: pqrst, .. },
            ComposerLine::Draft { text: uvwxy, .. },
            ComposerLine::Draft { text: z, .. },
        ] if fghij == "fghij"
            && klmno == "klmno"
            && pqrst == "pqrst"
            && uvwxy == "uvwxy"
            && z == "z"
    ));
}

#[test]
fn composer_cursor_tracks_upward_movement_out_of_bounds() {
    let mut draft = ComposerDraft::new();
    draft.insert_text("line1\nline2\nline3\nline4\nline5\nline6");
    draft.cursor = 0;
    draft.set_scroll_line(1);
    let snapshot = ComposerSnapshot::new(&draft);
    let options = ComposerRenderOptions::default();

    let rows = render_lines(&snapshot, &options, 80, 5);
    let position = cursor_position(&draft, 80, &options, 5);

    assert_eq!(position.visible_row, Some(0));
    assert!(matches!(
        rows.as_slice(),
        [
            ComposerLine::Draft { text: line1, .. },
            ComposerLine::Draft { text: line2, .. },
            ComposerLine::Draft { text: line3, .. },
            ComposerLine::Draft { text: line4, .. },
            ComposerLine::Draft {
                indicator: Some(OverflowIndicator::Below),
                text: line5,
                ..
            },
        ] if line1 == "line1"
            && line2 == "line2"
            && line3 == "line3"
            && line4 == "line4"
            && line5 == "line5"
    ));
}
