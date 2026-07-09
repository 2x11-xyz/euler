use super::*;
use crate::ui::visual_canvas::CursorTarget;

#[test]
fn question_mark_help_overlay_is_global_only_for_idle_composer() {
    let mut core = core();

    assert_eq!(
        core.handle_input(key(KeyCode::Char('?'))),
        CoreEffect::Render
    );
    assert!(matches!(core.modal, Some(Modal::Help)));
    assert_eq!(
        core.handle_input(key(KeyCode::Char('x'))),
        CoreEffect::Render
    );
    assert!(core.bottom.composer().submit_text().is_empty());
    assert!(core.modal.is_none());

    core.handle_input(key(KeyCode::Char('w')));
    assert_eq!(
        core.handle_input(key(KeyCode::Char('?'))),
        CoreEffect::Render
    );
    assert!(core.modal.is_none());
    assert_eq!(core.bottom.composer().submit_text(), "w?");

    let mut palette_core = super::core();
    palette_core.handle_input(key(KeyCode::Char('/')));
    assert_eq!(
        palette_core.handle_input(key(KeyCode::Char('?'))),
        CoreEffect::Render
    );
    let BottomOwner::Palette(palette) = palette_core.bottom.owner() else {
        panic!("palette should own input");
    };
    assert!(palette.input().contains('?'));
}

#[test]
fn question_mark_edits_next_draft_during_streaming_turn_without_opening_help() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    assert_eq!(
        core.handle_input(key(KeyCode::Char('?'))),
        CoreEffect::Render
    );
    assert!(core.modal.is_none());
    assert_eq!(core.bottom.composer().submit_text(), "?");
}

#[test]
fn empty_composer_prompt_has_breathing_room_above_statusline() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();

    terminal.draw(|frame| core.render(frame)).expect("draw");

    let contents = terminal.backend().screen_contents();
    let areas = layout(
        Rect::new(0, 0, 80, 24),
        core.composer_height(),
        core.notice_height(),
        core.permission_ask_height(80),
        core.activity_height(),
    );
    assert_eq!(core.composer_height(), 1);
    assert_eq!(areas.status.y, areas.bottom.y + 1);
    assert_eq!(areas.notice.height, 0);
    assert!(screen_row(&contents, areas.bottom.y).starts_with("▌ "));
    let status = screen_row(&contents, areas.status.y);
    assert!(status.starts_with("  ⏎ send · / commands · ctrl+o expand"));
    assert!(status.contains(" · echo · ctx ?% · "));
    assert!(!status.contains("Context ?% used"));
}

#[test]
fn startup_history_sits_immediately_above_prompt_status_in_compact_viewport() {
    let mut terminal = crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 24), 16)
        .expect("inline terminal");
    let mut core = core();

    render_compact_frame(&mut terminal, &mut core);

    let rows = terminal.backend().screen_rows();
    let equation = row_containing(&rows, "e^(iπ) + 1 = 0");
    let prompt = row_containing(&rows, "▌");
    let status = row_containing(&rows, "echo · ctx");
    assert_eq!(status, prompt + 2, "rows: {rows:?}");
    assert!(rows[prompt + 1].trim().is_empty(), "rows: {rows:?}");
    assert!(rows[prompt - 1].trim().is_empty(), "rows: {rows:?}");
    assert!(
        prompt.saturating_sub(equation) <= 5,
        "banner should finish directly above the transient row and footer, rows: {rows:?}"
    );
}

#[test]
fn ctrl_c_quit_notice_uses_reserved_transient_row_without_growing_frame() {
    let mut core = core();
    core.drain_finalized_visual_lines(80);

    let before = core.visual_canvas_frame(80);
    assert_eq!(
        core.handle_input(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        CoreEffect::Render
    );
    let after = core.visual_canvas_frame(80);

    assert_eq!(after.required_height, before.required_height);
    assert_eq!(
        after.active_frame_lines.len(),
        before.active_frame_lines.len()
    );
    assert!(
        after.active_frame_lines.iter().any(|line| {
            line.plain_text() == "ctrl+c again to quit · session saved, /resume restores"
        }),
        "lines: {:?}",
        after
            .active_frame_lines
            .iter()
            .map(crate::ui::visual_canvas::CanvasLine::plain_text)
            .collect::<Vec<_>>()
    );

    assert_eq!(
        core.handle_input(key(KeyCode::Char('/'))),
        CoreEffect::Render
    );
    let palette = core.visual_canvas_frame(80);
    assert!(
        !palette.active_frame_lines.iter().any(|line| {
            line.plain_text() == "ctrl+c again to quit · session saved, /resume restores"
        }),
        "lines: {:?}",
        palette
            .active_frame_lines
            .iter()
            .map(crate::ui::visual_canvas::CanvasLine::plain_text)
            .collect::<Vec<_>>()
    );
}

#[test]
fn transient_notice_composer_and_status_are_separated_by_blank_rows() {
    let mut core = core();
    core.notice = Some("copy failed: xclip exited with a non-zero status".to_owned());

    let lines = core
        .visual_canvas_frame(80)
        .active_frame_lines
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>();

    let notice = lines
        .iter()
        .position(|line| line.starts_with("copy failed:"))
        .expect("notice row");
    let prompt = lines
        .iter()
        .position(|line| line.starts_with("▌"))
        .expect("prompt row");
    let status = lines
        .iter()
        .position(|line| line.contains("echo · ctx"))
        .expect("status row");

    assert_eq!(prompt, notice + 2, "lines: {lines:?}");
    assert_eq!(status, prompt + 2, "lines: {lines:?}");
    assert!(lines[notice + 1].is_empty(), "lines: {lines:?}");
    assert!(lines[prompt + 1].is_empty(), "lines: {lines:?}");
}

#[test]
fn slash_palette_appends_below_prompt_and_status_without_moving_footer_prefix() {
    let mut core = core();
    core.drain_finalized_visual_lines(80);

    let before = core.visual_canvas_frame(80);
    assert_eq!(
        core.handle_input(key(KeyCode::Char('/'))),
        CoreEffect::Render
    );
    let after = core.visual_canvas_frame(80);
    let lines = after
        .active_frame_lines
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>();

    let prompt = lines
        .iter()
        .position(|line| line.starts_with('▌'))
        .expect("prompt row");
    let status = lines
        .iter()
        .position(|line| line.contains("echo · ctx"))
        .expect("status row");
    let slash = lines
        .iter()
        .position(|line| line.trim() == "\u{258c} /")
        .expect("slash input row");

    assert_eq!(status, prompt + 2, "lines: {lines:?}");
    assert!(lines[prompt + 1].is_empty(), "lines: {lines:?}");
    assert_eq!(slash, status + 1, "lines: {lines:?}");
    assert_eq!(
        after.cursor,
        Some(CursorTarget {
            row: u16::try_from(slash).expect("slash row fits"),
            column: 3,
        })
    );
    assert!(after.required_height > before.required_height);
    assert!(after.prefer_stable_height);
    assert!(!before.prefer_stable_height);
}

#[test]
fn repeated_finalized_writes_move_banner_up_without_gap_or_clipping() {
    let mut terminal = crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 24), 16)
        .expect("inline terminal");
    let mut core = core();
    render_compact_frame(&mut terminal, &mut core);

    write_finalized_line_and_render(&mut terminal, &mut core, "first generated");
    write_finalized_line_and_render(&mut terminal, &mut core, "second generated");

    let rows = terminal.backend().screen_rows();
    let equation = row_containing(&rows, "e^(iπ) + 1 = 0");
    let first = row_containing(&rows, "first generated");
    let second = row_containing(&rows, "second generated");
    let prompt = row_containing(&rows, "▌");
    assert!(equation < first, "rows: {rows:?}");
    // first, hairline, second — Warm Ledger places a dim rule under each block
    assert_eq!(second, first + 2, "rows: {rows:?}");
    assert!(rows[prompt - 1].trim().is_empty(), "rows: {rows:?}");
    // second, hairline, three breathing blanks, prompt
    assert_eq!(prompt, second + 5, "rows: {rows:?}");
}

#[test]
fn logical_history_reflows_when_canvas_width_changes() {
    let mut core = core();
    core.drain_finalized_visual_lines(80);
    let sentence = "Provider abstraction, permissions, provenance, and transcript composition should reflow when the terminal width changes.";
    core.push_finalized_visual_item(TranscriptItem::AssistantMessage(sentence.to_owned()));

    let narrow = core
        .drain_finalized_visual_lines(36)
        .into_iter()
        .map(|line| line.plain_text())
        .collect::<Vec<_>>();
    let wide = core
        .drain_finalized_visual_lines(120)
        .into_iter()
        .map(|line| line.plain_text())
        .collect::<Vec<_>>();
    let wide_text = wide.join("\n");
    let wide_words = wide_text.split_whitespace().collect::<Vec<_>>().join(" ");

    assert!(
        narrow.len() > wide.len(),
        "narrow history should occupy more rows after reflow: narrow={narrow:?}, wide={wide:?}"
    );
    assert!(
        wide_words.contains(sentence),
        "wide history should preserve complete prose content: {wide_text:?}"
    );
}

#[test]
fn permission_approval_and_tool_history_stay_compact_after_inline_ask() {
    let mut terminal = crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 24), 16)
        .expect("inline terminal");
    let mut core = core();
    render_compact_frame(&mut terminal, &mut core);

    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_CALL,
        object([
            ("id", "call-run".into()),
            ("name", "run_shell".into()),
            (
                "input",
                serde_json::json!({"command": "bash -lc 'cargo check'"}),
            ),
        ]),
    )));
    render_compact_frame(&mut terminal, &mut core);
    core.modal = Some(Modal::Permission(PermissionRequest::new(
        Capability::ShellExec,
        "tool run_shell".to_owned(),
    )));
    render_compact_frame(&mut terminal, &mut core);
    assert!(terminal
        .backend()
        .screen_contents()
        .contains("Approval required"));

    assert_eq!(
        core.handle_input(key(KeyCode::Char('y'))),
        CoreEffect::Render
    );
    assert_eq!(reply_rx.recv().expect("reply"), PermissionReply::AllowOnce);
    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::PERMISSION_DECISION,
            object([
                ("capability", "shell-exec".into()),
                ("mode", "ask".into()),
                ("allowed", true.into()),
                ("decision", "allowed".into()),
            ]),
        ),
    );
    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::TOOL_RESULT,
            object([
                ("id", "call-run".into()),
                ("name", "run_shell".into()),
                ("ok", true.into()),
                ("output", "finished".into()),
            ]),
        ),
    );

    let rows = terminal.backend().scrollback_rows();
    assert!(!rows.iter().any(|row| row.contains("Approval required")));
    assert!(!rows.iter().any(|row| row.contains("y  Allow once")));
    let decision = row_containing(&rows, "✓ allowed once · shell-exec");
    let tool = row_containing(&rows, "bash $ cargo check");
    let blank_rows_between = rows[decision + 1..tool]
        .iter()
        .filter(|row| row.trim().is_empty())
        .count();
    assert!(
        blank_rows_between <= 1,
        "approval/tool gap should stay compact, rows: {rows:?}"
    );
}

#[test]
fn scrollback_preserves_banner_user_tool_and_final_after_many_insertions() {
    let mut terminal = crate::ui::terminal::InlineTerminal::new(VT100Backend::new(72, 24), 16)
        .expect("inline terminal");
    let mut core = core();
    render_compact_frame(&mut terminal, &mut core);

    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::USER_MESSAGE,
            object([("content", "inspect".into())]),
        ),
    );
    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::TOOL_CALL,
            object([
                ("id", "call-read".into()),
                ("name", "read_file".into()),
                ("input", serde_json::json!({"path": "AGENTS.md"})),
            ]),
        ),
    );
    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::TOOL_RESULT,
            object([
                ("id", "call-read".into()),
                ("name", "read_file".into()),
                ("ok", true.into()),
                ("output", "raw".into()),
            ]),
        ),
    );
    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::ASSISTANT_MESSAGE,
            object([("content", "final prose".into())]),
        ),
    );
    for index in 0..12 {
        write_finalized_line_and_render(&mut terminal, &mut core, &format!("filler {index}"));
    }

    let rows = terminal.backend().scrollback_rows();
    assert_ordered(
        &rows,
        &[
            "e^(iπ) + 1 = 0",
            "▌ inspect",
            "explore",
            "Read AGENTS.md",
            "final prose",
            "filler 11",
        ],
    );
}

#[test]
fn finalized_visual_output_accumulates_worked_duration_separators() {
    let mut core = core();
    core.drain_finalized_visual_lines(72);

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::USER_MESSAGE,
        object([("content", "first".into())]),
    )));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "first answer".into())]),
    )));
    core.handle_turn_outcome(TurnOutcome::Complete, Some(Duration::from_secs(65)));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::USER_MESSAGE,
        object([("content", "second".into())]),
    )));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "second answer".into())]),
    )));
    core.handle_turn_outcome(TurnOutcome::Complete, Some(Duration::from_secs(5)));

    let text = drain_finalized_visual_text(&mut core, 72);
    assert!(text.contains("first answer"));
    assert!(text.contains("second answer"));
    assert_eq!(text.matches("Worked for").count(), 2);
    assert!(text.contains("Worked for 1m 5s"));
    assert!(text.contains("Worked for 5s"));
    assert!(!text.contains(&"─".repeat(72)));
}

#[test]
fn finalized_visual_output_suppresses_short_worked_duration() {
    let mut core = core();
    core.drain_finalized_visual_lines(40);

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::USER_MESSAGE,
        object([("content", "quick".into())]),
    )));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "quick answer".into())]),
    )));
    core.handle_turn_outcome(TurnOutcome::Complete, Some(Duration::from_secs(4)));

    let text = drain_finalized_visual_text(&mut core, 40);
    assert!(text.contains("quick answer"));
    assert!(!text.contains("Worked for"));
}

#[test]
fn finalized_prompt_and_answer_batches_keep_one_rhythm_row() {
    let mut core = core();
    core.drain_finalized_visual_lines(40);

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::USER_MESSAGE,
        object([("content", "hi".into())]),
    )));
    let user_lines = core
        .drain_finalized_visual_lines(40)
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>();
    let user_row = user_lines
        .iter()
        .position(|line| line.contains("▌ hi"))
        .expect("user row");
    // Hairline under the user block is the Warm Ledger rhythm row.
    assert!(
        user_lines
            .get(user_row + 1)
            .is_some_and(|line| line.contains('─')),
        "user_lines: {user_lines:?}"
    );

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "Hi! How can I help?".into())]),
    )));
    let answer_lines = core
        .drain_finalized_visual_lines(40)
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>();
    let answer_row = answer_lines
        .iter()
        .position(|line| line.contains("Hi! How can I help?"))
        .expect("answer row");
    assert!(
        answer_lines
            .get(answer_row + 1)
            .is_some_and(|line| line.contains('─')),
        "answer_lines: {answer_lines:?}"
    );

    core.handle_turn_outcome(TurnOutcome::Complete, Some(Duration::from_secs(4)));
    let after_outcome = drain_finalized_visual_text(&mut core, 40);
    assert!(after_outcome.contains("Hi! How can I help?"));
    assert!(!after_outcome.contains("Worked for"));
}

#[test]
fn finalized_wrapped_prompt_uses_continuous_user_rail() {
    let theme = Theme::default();
    let lines = render_finalized_visual_items(
        &[TranscriptItem::UserMessage(
            "alpha beta gamma delta epsilon".to_owned(),
        )],
        &theme,
        28,
        TOOL_CALL_MAX_LINES,
        &std::collections::HashSet::new(),
    )
    .iter()
    .map(crate::ui::visual_canvas::CanvasLine::plain_text)
    .collect::<Vec<_>>();

    assert!(
        lines.iter().any(|line| line.contains("▌ alpha beta gamma")),
        "lines: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("▌") && (line.contains("epsilon") || line.contains("delta"))),
        "lines: {lines:?}"
    );
    assert_eq!(
        lines.iter().filter(|line| line.contains("▌")).count(),
        2,
        "wrapped prompt should keep a continuous user rail: {lines:?}"
    );
}

#[test]
fn finalized_multi_column_markdown_tables_render_grid_or_stack_by_width() {
    let theme = Theme::default();
    let table = "| Layer | Responsibility | Repo location |\n|---|---|---|\n| CLI/TUI layer | User-facing command-line and Ratatui transcript composer status UX | euler-cli |\n";

    let narrow = render_finalized_visual_items(
        &[TranscriptItem::AssistantMessage(table.to_owned())],
        &theme,
        44,
        TOOL_CALL_MAX_LINES,
        &std::collections::HashSet::new(),
    )
    .iter()
    .map(crate::ui::visual_canvas::CanvasLine::plain_text)
    .collect::<Vec<_>>();

    assert!(
        narrow
            .iter()
            .any(|line| line.trim_start() == "Layer: CLI/TUI layer"),
        "stacked table row missing at narrow width: {narrow:?}"
    );
    assert!(
        narrow
            .iter()
            .any(|line| line.trim_start() == "Repo location: euler-cli"),
        "stacked repo row missing at narrow width: {narrow:?}"
    );
    assert!(
        narrow.iter().all(|line| !line.contains('━')),
        "narrow multi-column table should not render as a grid: {narrow:?}"
    );
    assert!(
        narrow
            .iter()
            .all(|line| crate::ui::text::display_width(line) <= 44),
        "narrow stacked table overflowed: {narrow:?}"
    );

    let wide = render_finalized_visual_items(
        &[TranscriptItem::AssistantMessage(table.to_owned())],
        &theme,
        100,
        TOOL_CALL_MAX_LINES,
        &std::collections::HashSet::new(),
    )
    .iter()
    .map(crate::ui::visual_canvas::CanvasLine::plain_text)
    .collect::<Vec<_>>();

    assert!(
        wide.iter().any(|line| line.contains("CLI/TUI layer")),
        "wide grid table row missing: {wide:?}"
    );
    assert!(
        wide.iter().any(|line| line.contains('━')),
        "wide table should render as a grid: {wide:?}"
    );
    assert!(
        wide.iter().all(|line| {
            let trimmed = line.trim_start();
            trimmed != "Layer: CLI/TUI layer" && trimmed != "Repo location: euler-cli"
        }),
        "wide table should not include stacked rows: {wide:?}"
    );
    assert!(
        wide.iter()
            .all(|line| crate::ui::text::display_width(line) <= 100),
        "wide grid table overflowed: {wide:?}"
    );
}

#[test]
fn finalized_multi_column_table_stays_stacked_after_terminal_resize() {
    let mut terminal = crate::ui::terminal::InlineTerminal::new(VT100Backend::new(100, 24), 24)
        .expect("inline terminal");
    let mut core = core();
    core.push_finalized_visual_item(TranscriptItem::AssistantMessage(
        "| Layer | Responsibility | Repo location |\n|---|---|---|\n| CLI/TUI layer | User-facing command-line and Ratatui transcript composer status UX | euler-cli |\n".to_owned(),
    ));

    render_compact_frame(&mut terminal, &mut core);
    terminal.backend_mut().resize(44, 24);
    render_compact_frame(&mut terminal, &mut core);

    let rows = terminal
        .backend()
        .screen_rows()
        .into_iter()
        .map(|row| row.trim_end().to_owned())
        .collect::<Vec<_>>();
    assert!(
        rows.iter()
            .any(|row| row.trim_start() == "Layer: CLI/TUI layer"),
        "stacked table row missing after resize: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.trim_start() == "Repo location: euler-cli"),
        "stacked repo row missing after resize: {rows:?}"
    );
    assert!(
        rows.iter().all(|row| !row.contains('━')),
        "resized multi-column table should not leave grid artifacts: {rows:?}"
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.contains("CLI/TUI layer"))
            .count(),
        1,
        "resized table should not duplicate body rows: {rows:?}"
    );
}

#[test]
fn finalized_multi_item_batches_keep_single_internal_and_trailing_rhythm() {
    let theme = Theme::default();
    let lines = render_finalized_visual_items(
        &[
            TranscriptItem::UserMessage("hi".to_owned()),
            TranscriptItem::AssistantMessage("Hi! How can I help?".to_owned()),
            TranscriptItem::WorkedDuration("5s".to_owned()),
        ],
        &theme,
        80,
        TOOL_CALL_MAX_LINES,
        &std::collections::HashSet::new(),
    )
    .iter()
    .map(crate::ui::visual_canvas::CanvasLine::plain_text)
    .collect::<Vec<_>>();

    assert!(
        lines.first().is_some_and(|line| line.contains("▌ hi")),
        "lines: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Hi! How can I help?")),
        "lines: {lines:?}"
    );
    assert!(lines.iter().any(|line| line.contains("Worked for 5s")));
    assert_eq!(lines.last().map(String::as_str), Some(""));
    assert!(
        !lines
            .windows(2)
            .any(|pair| pair[0].is_empty() && pair[1].is_empty()),
        "unexpected blank canyon: {lines:?}"
    );
}

#[test]
fn finalized_tool_batches_do_not_get_prompt_answer_trailing_rhythm() {
    let theme = Theme::default();
    let lines = render_finalized_visual_items(
        &[TranscriptItem::ToolRun {
            command: "ls -la".to_owned(),
            ok: true,
            error: String::new(),
            output: "exit 0\nfile".to_owned(),
            exit_code: Some(0),
        }],
        &theme,
        80,
        TOOL_CALL_MAX_LINES,
        &std::collections::HashSet::new(),
    )
    .iter()
    .map(crate::ui::visual_canvas::CanvasLine::plain_text)
    .collect::<Vec<_>>();

    assert!(lines.iter().any(|line| line.contains("bash $ ls -la")));
    assert_ne!(lines.last().map(String::as_str), Some(""));
}

#[test]
fn finalized_tool_output_batch_separates_following_assistant_prose() {
    let mut terminal = crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 14), 14)
        .expect("inline terminal");
    let mut core = core();
    core.drain_finalized_visual_lines(80);

    core.push_finalized_visual_item(TranscriptItem::ToolRun {
        command: "printf dashes".to_owned(),
        ok: true,
        error: String::new(),
        output: "last tool output row".to_owned(),
        exit_code: Some(0),
    });
    render_compact_frame(&mut terminal, &mut core);

    core.push_finalized_visual_item(TranscriptItem::AssistantMessage(
        "I see 16 em dashes in the output.".to_owned(),
    ));
    render_compact_frame(&mut terminal, &mut core);

    let rows = terminal.backend().scrollback_rows();
    let tool_last = row_containing(&rows, "last tool output row");
    assert!(rows.iter().any(|row| row.contains("exit 0 · 1 line")));
    // Hairline under the tool block separates it from following assistant prose.
    assert!(rows[tool_last + 1].contains('─'), "rows: {rows:?}");
    assert!(
        rows[tool_last + 2].contains("I see 16 em dashes"),
        "rows: {rows:?}"
    );
}

#[test]
fn finalized_visual_output_includes_worked_duration_at_threshold() {
    let mut core = core();
    core.drain_finalized_visual_lines(40);

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::USER_MESSAGE,
        object([("content", "threshold".into())]),
    )));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "threshold answer".into())]),
    )));
    core.handle_turn_outcome(TurnOutcome::Complete, Some(Duration::from_secs(5)));

    let text = drain_finalized_visual_text(&mut core, 40);
    assert!(text.contains("threshold answer"));
    assert!(text.contains("Worked for 5s"));
}

#[test]
fn activity_cells_accumulate_before_final_answer() {
    let mut terminal = crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 12), 12)
        .expect("inline terminal");
    let mut core = core();
    render_compact_frame(&mut terminal, &mut core);

    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::TOOL_CALL,
            object([
                ("id", "call-read".into()),
                ("name", "read_file".into()),
                ("input", serde_json::json!({"path": "Cargo.toml"})),
            ]),
        ),
    );
    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::TOOL_RESULT,
            object([
                ("id", "call-read".into()),
                ("name", "read_file".into()),
                ("ok", true.into()),
                ("output", "raw".into()),
            ]),
        ),
    );
    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::TOOL_CALL,
            object([
                ("id", "call-run".into()),
                ("name", "run_shell".into()),
                (
                    "input",
                    serde_json::json!({"command": "bash -lc 'cargo test'"}),
                ),
            ]),
        ),
    );
    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::TOOL_RESULT,
            object([
                ("id", "call-run".into()),
                ("name", "run_shell".into()),
                ("ok", true.into()),
                ("output", "ok".into()),
            ]),
        ),
    );

    let before_final = terminal.backend().scrollback_rows();
    assert_ordered(
        &before_final,
        &["explore", "Read Cargo.toml", "bash $ cargo test"],
    );
    assert!(!before_final.iter().any(|row| row.contains("final answer")));

    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::ASSISTANT_MESSAGE,
            object([("content", "final answer".into())]),
        ),
    );
    let after_final = terminal.backend().scrollback_rows();
    assert_ordered(
        &after_final,
        &["explore", "bash $ cargo test", "final answer"],
    );
}

#[test]
fn tool_round_limit_finalizes_guidance_without_raw_session_failure() {
    let mut terminal = crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 12), 12)
        .expect("inline terminal");
    let mut core = core();
    render_compact_frame(&mut terminal, &mut core);
    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::TOOL_RESULT,
            object([
                ("name", "run_shell".into()),
                ("ok", false.into()),
                ("error", "exit".into()),
                ("output", "partial work".into()),
            ]),
        ),
    );
    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::ASSISTANT_MESSAGE,
            object([(
                "content",
                "Exploration limit reached; here is what I found so far. Send a follow-up to continue from this point.".into(),
            )]),
        ),
    );
    core.handle_turn_outcome(TurnOutcome::Complete, Some(Duration::from_secs(11)));
    render_compact_frame(&mut terminal, &mut core);

    let rows = terminal.backend().scrollback_rows();
    assert_ordered(
        &rows,
        &[
            "partial work",
            "Exploration limit reached",
            "Worked for 11s",
        ],
    );
    assert!(!rows
        .iter()
        .any(|row| row.contains("session: model exceeded maximum tool rounds")));
    assert!(!rows
        .iter()
        .any(|row| row.contains("run_turn: model exceeded maximum tool rounds")));
    let screen = terminal.backend().screen_contents();
    assert!(!screen.contains("⠧ working"));
    assert!(!screen.contains("turn failed"));
    assert!(screen.contains("▌"));
}

#[test]
fn permission_denial_returns_failed_tool_result_not_interruption() {
    let mut terminal = crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 12), 12)
        .expect("inline terminal");
    let mut core = core();
    render_compact_frame(&mut terminal, &mut core);
    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::PERMISSION_DECISION,
            object([
                ("capability", "shell-exec".into()),
                ("mode", "ask".into()),
                ("allowed", false.into()),
                ("decision", "denied".into()),
            ]),
        ),
    );
    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::TOOL_RESULT,
            object([
                ("id", "call-denied".into()),
                ("name", "run_shell".into()),
                ("ok", false.into()),
                ("error", "permission denied".into()),
            ]),
        ),
    );
    core.handle_turn_outcome(TurnOutcome::Complete, None);
    render_compact_frame(&mut terminal, &mut core);

    let rows = terminal.backend().scrollback_rows();
    let rendered = rows.join("\n");
    assert!(rendered.contains("permission denied"));
    assert!(!rows
        .iter()
        .any(|row| row.contains("Permission was denied for shell-exec")));
    let screen = terminal.backend().screen_contents();
    assert!(!screen.contains("interrupted — tell euler what to do differently"));
    assert!(screen.contains("▌"));
}

#[test]
fn in_flight_error_frame_is_failed_not_working_or_prompt_ready() {
    let mut terminal = crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 12), 12)
        .expect("inline terminal");
    let mut core = core();
    render_compact_frame(&mut terminal, &mut core);
    let AppState::Idle { session } = std::mem::replace(&mut core.state, AppState::Empty) else {
        panic!("core should start idle");
    };
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now() - Duration::from_secs(2),
    };

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_RESULT,
        object([
            ("name", "run_shell".into()),
            ("ok", true.into()),
            ("output", "partial work".into()),
        ]),
    )));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::ERROR,
        object([
            ("source", "provider".into()),
            ("message", "transport down".into()),
        ]),
    )));
    render_compact_frame(&mut terminal, &mut core);

    let failed_gap = terminal.backend().screen_contents();
    assert!(failed_gap.contains("provider: transport down"));
    assert!(failed_gap.contains("■ turn failed — waiting for cleanup"));
    assert!(!failed_gap.contains("⠧ working"));
    assert!(
        !terminal
            .backend()
            .screen_rows()
            .iter()
            .any(|row| row.starts_with("▌")),
        "failed in-flight frame should not look prompt-ready: {failed_gap:?}"
    );

    core.handle_turn_event(TurnEvent::TurnDone {
        outcome: TurnOutcome::Failed("transport down".to_owned()),
        session,
    });
    render_compact_frame(&mut terminal, &mut core);

    let history = terminal.backend().scrollback_rows().join("\n");
    assert!(history.contains("partial work"));
    assert_eq!(history.matches("provider: transport down").count(), 1);
    assert!(!history.contains("run_turn: transport down"));
    let done = terminal.backend().screen_contents();
    assert!(!done.contains("■ Turn failed"));
    assert!(!done.contains("⠧ working"));
    assert!(done.contains("▌"));
}

#[test]
fn failed_outcome_without_error_event_restores_prompt_after_turn_done() {
    let mut terminal = crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 12), 12)
        .expect("inline terminal");
    let mut core = core();
    render_compact_frame(&mut terminal, &mut core);
    let AppState::Idle { session } = std::mem::replace(&mut core.state, AppState::Empty) else {
        panic!("core should start idle");
    };
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now() - Duration::from_secs(2),
    };

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_RESULT,
        object([
            ("name", "run_shell".into()),
            ("ok", true.into()),
            ("output", "partial work".into()),
        ]),
    )));
    render_compact_frame(&mut terminal, &mut core);
    let before_done = terminal.backend().screen_contents();
    assert!(before_done.contains("⠧ working"));
    assert!(!before_done.contains("■ Turn failed"));

    core.handle_turn_event(TurnEvent::TurnDone {
        outcome: TurnOutcome::Failed("transport down".to_owned()),
        session,
    });
    render_compact_frame(&mut terminal, &mut core);

    let history = terminal.backend().scrollback_rows().join("\n");
    assert!(history.contains("partial work"));
    assert!(history.contains("run_turn: transport down"));
    let done = terminal.backend().screen_contents();
    assert!(!done.contains("■ Turn failed"));
    assert!(!done.contains("⠧ working"));
    assert!(done.contains("▌"));
}

#[test]
fn final_completion_collapses_live_viewport_without_blank_gap() {
    let mut terminal = crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 42), 16)
        .expect("inline terminal");
    let mut core = core();
    render_compact_frame(&mut terminal, &mut core);
    let AppState::Idle { session } = std::mem::replace(&mut core.state, AppState::Empty) else {
        panic!("core should start idle");
    };
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now() - Duration::from_secs(41),
    };

    render_compact_frame(&mut terminal, &mut core);

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_RESULT,
        object([("content", "final answer".into())]),
    )));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "final answer".into())]),
    )));
    core.handle_turn_event(TurnEvent::TurnDone {
        outcome: TurnOutcome::Complete,
        session,
    });
    render_compact_frame(&mut terminal, &mut core);

    let rows = terminal.backend().screen_rows();
    let final_answer = row_containing(&rows, "final answer");
    let worked = row_containing(&rows, "Worked for 41s");
    let recap = row_containing(&rows, "0 files · ctx");
    let prompt = row_containing(&rows, "▌");
    let status = row_containing(&rows, "echo · ctx");
    assert!(final_answer < worked, "rows: {rows:?}");
    assert!(worked < recap, "recap follows worked-for, rows: {rows:?}");
    assert!(
        prompt.saturating_sub(recap) <= 4,
        "final recap row should be adjacent to prompt/status, rows: {rows:?}"
    );
    assert!(rows[prompt - 1].trim().is_empty(), "rows: {rows:?}");
    assert_eq!(status, prompt + 2, "rows: {rows:?}");
    assert!(rows[prompt + 1].trim().is_empty(), "rows: {rows:?}");
}

#[test]
fn working_timer_marks_dirty_once_per_elapsed_second() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now() - Duration::from_secs(5),
    };

    assert!(core.mark_working_timer_dirty());
    assert!(!core.mark_working_timer_dirty());

    let AppState::TurnInFlight { started_at, .. } = &mut core.state else {
        panic!("turn should still be in flight");
    };
    *started_at = Instant::now() - Duration::from_secs(7);

    assert!(core.mark_working_timer_dirty());
    core.state = AppState::Empty;
    assert!(!core.mark_working_timer_dirty());
    assert_eq!(core.last_working_elapsed_secs, None);
}

#[test]
fn banner_is_drained_as_history_not_fixed_chrome() {
    let mut core = core();
    let banner = core.drain_finalized_visual_lines(80);

    let rendered = banner
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("e^(iπ) + 1 = 0"));
    assert!(core
        .drain_finalized_visual_lines(120)
        .iter()
        .any(|line| line.plain_text().contains("e^(iπ) + 1 = 0")));
}

#[test]
fn finalized_visual_output_renders_in_logical_canvas_without_active_duplicate() {
    let mut core = core();

    let frame = core.render_visual_canvas(80);
    let text = frame
        .active_frame_lines
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("e^(iπ) + 1 = 0"));
    assert!(text.contains("▌"));
    assert!(text.contains("echo · ctx"));

    let second = core.render_visual_canvas(80);
    let second_text = second
        .active_frame_lines
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(second_text.matches("e^(iπ) + 1 = 0").count(), 1);
}

#[test]
fn finalized_visual_state_survives_terminal_write_errors() {
    let mut core = core();
    core.drain_finalized_visual_lines(80);
    core.push_finalized_visual_item(TranscriptItem::AssistantMessage("preserve me".to_owned()));

    let text = drain_finalized_visual_text(&mut core, 80);
    assert!(
        text.contains("preserve me"),
        "finalized visual text lost: {text:?}"
    );
}

#[test]
fn active_turn_finalized_history_can_commit_to_native_scrollback() {
    let mut idle_core = core();
    idle_core.push_finalized_visual_item(TranscriptItem::AssistantMessage(
        "one\ntwo\nthree\nfour".to_owned(),
    ));
    let idle_frame = idle_core.render_visual_canvas(24);
    assert!(
        idle_frame.committable_rows > 0,
        "idle history should be eligible for native scrollback commit"
    );

    let mut active_core = core();
    active_core.push_finalized_visual_item(TranscriptItem::AssistantMessage(
        "one\ntwo\nthree\nfour".to_owned(),
    ));
    let (_tx, worker_rx) = mpsc::channel();
    active_core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    let active_frame = active_core.render_visual_canvas(24);
    assert!(
        active_frame.committable_rows > 0,
        "active finalized history should be eligible for native scrollback"
    );
    assert!(
        active_frame.committable_rows <= active_frame.history_rows,
        "active turns without stable live markdown may commit only finalized history"
    );
}

fn render_compact_frame(
    terminal: &mut crate::ui::terminal::InlineTerminal<VT100Backend>,
    core: &mut AppCore,
) {
    let width = terminal.size().expect("terminal size").width;
    let frame = core.render_visual_canvas(width);
    terminal
        .draw_visual_frame(&frame)
        .expect("draw visual frame");
}

fn write_finalized_line_and_render(
    terminal: &mut crate::ui::terminal::InlineTerminal<VT100Backend>,
    core: &mut AppCore,
    line: &str,
) {
    core.push_finalized_visual_item(TranscriptItem::AssistantMessage(line.to_owned()));
    let width = terminal.size().expect("terminal size").width;
    let frame = core.visual_canvas_frame(width);
    terminal
        .draw_visual_frame(&frame)
        .expect("draw visual frame");
}

fn insert_event_and_render(
    terminal: &mut crate::ui::terminal::InlineTerminal<VT100Backend>,
    core: &mut AppCore,
    event: EventEnvelope,
) {
    core.handle_turn_event(TurnEvent::Event(event));
    render_compact_frame(terminal, core);
}

fn row_containing(rows: &[String], needle: &str) -> usize {
    rows.iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("expected row containing {needle:?}, rows: {rows:?}"))
}

fn assert_ordered(rows: &[String], expected: &[&str]) {
    let mut cursor = 0usize;
    for needle in expected {
        let Some(relative) = rows[cursor..].iter().position(|row| row.contains(needle)) else {
            panic!("expected {needle:?} after row {cursor}, rows: {rows:?}");
        };
        cursor += relative + 1;
    }
}

#[test]
fn finalized_visual_output_uses_worked_separator_as_turn_boundary() {
    let mut core = core();
    core.drain_finalized_visual_lines(40);

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::USER_MESSAGE,
        object([("content", "first".into())]),
    )));
    let first = core
        .drain_finalized_visual_lines(40)
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<String>();
    assert!(!first.contains(&"─".repeat(40)));

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "done".into())]),
    )));
    core.handle_turn_outcome(TurnOutcome::Complete, Some(Duration::from_secs(5)));
    let completed = core
        .drain_finalized_visual_lines(40)
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<String>();
    assert!(completed.contains("Worked for 5s"));

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::USER_MESSAGE,
        object([("content", "second".into())]),
    )));

    let second_lines = core
        .drain_finalized_visual_lines(40)
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>();
    let second = second_lines.join("");
    // Full-frame drain still shows the prior Worked separator as the turn
    // boundary; the new user row must follow it, and it must not be re-emitted.
    assert_eq!(
        second.matches("Worked for").count(),
        1,
        "second_lines: {second_lines:?}"
    );
    let worked = second_lines
        .iter()
        .position(|line| line.contains("Worked for"))
        .expect("worked separator");
    let second_user = second_lines
        .iter()
        .position(|line| line.contains("second"))
        .expect("second user");
    assert!(worked < second_user, "second_lines: {second_lines:?}");
}

#[test]
fn no_newline_stream_finalizes_to_history_without_live_duplicate() {
    let mut core = core();
    core.drain_finalized_visual_lines(80);
    let mut terminal =
        crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 12), 12).expect("terminal");

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "single line".into())]),
    )));
    assert_eq!(
        core.transcript.live_items(),
        vec![TranscriptItem::AssistantMessage("single line".to_owned())]
    );

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_RESULT,
        object([("content", "single line".into())]),
    )));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "single line".into())]),
    )));

    assert!(core.transcript.live_items().is_empty());
    let rendered = core
        .drain_finalized_visual_lines(80)
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(rendered.matches("single line").count(), 1);
    assert!(!rendered.contains("Model result"));

    // A PTY capture can include both an earlier live streaming frame and
    // the final inserted history row. The app invariant is stricter than
    // the capture artifact: after finalization, live state is empty and the
    // finalized answer drains into history once.
    terminal.draw(|frame| core.render(frame)).expect("draw");
    let live_viewport = terminal.backend().screen_contents();
    assert!(!live_viewport.contains("single line"));
    assert!(!live_viewport.contains("Model result"));
}

#[test]
fn streamed_answer_final_screen_contains_one_final_history_row() {
    let mut core = core();
    core.drain_finalized_visual_lines(80);
    let mut terminal =
        crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 12), 12).expect("terminal");

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "final answer\n".into())]),
    )));
    render_compact_frame(&mut terminal, &mut core);
    assert_eq!(
        terminal
            .backend()
            .screen_contents()
            .matches("final answer")
            .count(),
        1
    );

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_RESULT,
        object([("content", "final answer".into())]),
    )));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "final answer".into())]),
    )));

    let lines = core.drain_finalized_visual_lines(80);
    let rendered_history = lines
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(rendered_history.matches("final answer").count(), 1);
    terminal
        .write_finalized_lines(&lines)
        .expect("write finalized history");
    let frame = core.visual_canvas_frame(80);
    terminal.draw_visual_frame(&frame).expect("final draw");

    let contents = terminal.backend().screen_contents();
    assert!(contents.matches("final answer").count() <= 1);
    assert!(!contents.contains("Model result"));
}

#[test]
fn markdown_stream_handoff_keeps_heading_owned_by_one_surface() {
    let mut core = core();
    core.drain_finalized_visual_lines(80);
    let mut terminal =
        crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 18), 14).expect("terminal");
    let chunks = [
        "# Euler CLI Repo Summary\n",
        "\nThe CLI owns the transcript and active canvas.\n",
        "\n## Streaming Notes\n",
        "- live output stays transient\n",
        "- final output enters history once\n",
    ];
    let final_markdown = chunks.concat();

    for chunk in chunks {
        core.handle_turn_event(TurnEvent::Event(event(
            EventKind::MODEL_DELTA,
            object([("kind", "text".into()), ("delta", chunk.into())]),
        )));
        render_compact_frame(&mut terminal, &mut core);
        let screen = terminal.backend().screen_contents();
        assert!(
            screen.matches("Euler CLI Repo Summary").count() <= 1,
            "live frame duplicated heading: {screen:?}"
        );
    }

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_RESULT,
        object([("content", final_markdown.clone().into())]),
    )));
    render_compact_frame(&mut terminal, &mut core);
    let result_gap = terminal.backend().screen_contents();
    assert_eq!(
        result_gap.matches("Euler CLI Repo Summary").count(),
        1,
        "MODEL_RESULT gap lost or duplicated heading: {result_gap:?}"
    );

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", final_markdown.into())]),
    )));
    render_compact_frame(&mut terminal, &mut core);

    assert!(core.transcript.live_items().is_empty());
    let active = core
        .visual_canvas_frame(80)
        .active_frame_lines
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(active.matches("Euler CLI Repo Summary").count(), 1);
}

#[test]
fn markdown_stream_handoff_keeps_table_owned_by_one_surface() {
    let mut core = core();
    core.drain_finalized_visual_lines(80);
    let mut terminal =
        crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 18), 14).expect("terminal");
    let chunks = [
        "| Alpha | Beta |\n",
        "|---|---|\n",
        "| one | two |\n",
        "| three | four |\n",
    ];
    let final_markdown = chunks.concat();

    for chunk in chunks {
        core.handle_turn_event(TurnEvent::Event(event(
            EventKind::MODEL_DELTA,
            object([("kind", "text".into()), ("delta", chunk.into())]),
        )));
        render_compact_frame(&mut terminal, &mut core);
        let screen = terminal.backend().screen_contents();
        assert!(
            screen.matches("Alpha").count() <= 1,
            "live frame duplicated table header: {screen:?}"
        );
        assert!(
            screen.matches("one").count() <= 1,
            "live frame duplicated table body: {screen:?}"
        );
    }

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_RESULT,
        object([("content", final_markdown.clone().into())]),
    )));
    render_compact_frame(&mut terminal, &mut core);
    let result_gap = terminal.backend().screen_contents();
    assert_eq!(
        result_gap.matches("Alpha").count(),
        1,
        "MODEL_RESULT gap lost or duplicated table: {result_gap:?}"
    );

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", final_markdown.into())]),
    )));
    render_compact_frame(&mut terminal, &mut core);

    assert!(core.transcript.live_items().is_empty());
    let active = core
        .visual_canvas_frame(80)
        .active_frame_lines
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(active.matches("Alpha").count(), 1);
    assert_eq!(active.matches("one").count(), 1);
}

#[test]
fn idle_frame_does_not_render_stale_tool_activity_after_final_answer() {
    let mut core = core();
    core.drain_finalized_visual_lines(80);
    let mut terminal = Terminal::new(VT100Backend::new(80, 14)).expect("terminal");

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_CALL,
        object([
            ("id", "call-read".into()),
            ("name", "read_file".into()),
            ("input", serde_json::json!({"path": "AGENTS.md"})),
        ]),
    )));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_RESULT,
        object([
            ("id", "call-read".into()),
            ("name", "read_file".into()),
            ("ok", true.into()),
            ("output", "# raw instructions".into()),
        ]),
    )));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_RESULT,
        object([("content", "final answer".into())]),
    )));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "final answer".into())]),
    )));

    terminal.draw(|frame| core.render(frame)).expect("draw");
    let contents = terminal.backend().screen_contents();
    assert!(!contents.contains("read_file call"));
    assert!(!contents.contains("read_file completed"));
    assert!(!contents.contains("explore"));
    assert!(!contents.contains("# raw instructions"));
}

#[test]
fn patch_approval_hides_completed_read_file_activity() {
    let mut core = core();
    core.drain_finalized_visual_lines(80);
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_CALL,
        object([
            ("id", "call-read".into()),
            ("name", "read_file".into()),
            (
                "input",
                serde_json::json!({"path": "crates/euler-cli/src/ui/transcript.rs"}),
            ),
        ]),
    )));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_RESULT,
        object([
            ("id", "call-read".into()),
            ("name", "read_file".into()),
            ("ok", true.into()),
            ("output", "raw transcript source".into()),
        ]),
    )));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_CALL,
        object([
            ("id", "call-edit".into()),
            ("name", "edit_file".into()),
            (
                "input",
                serde_json::json!({"path": "crates/euler-cli/src/ui/transcript.rs"}),
            ),
        ]),
    )));
    core.modal = Some(patch_modal(diff_preview("alpha\n", "beta\n")));

    terminal
        .draw(|frame| core.render(frame))
        .expect("patch approval draw");
    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("Approval required"));
    assert!(!contents.contains("read_file call"));
    assert!(!contents.contains("read_file completed"));
    assert!(!contents.contains("raw transcript source"));
}

#[test]
fn patch_approval_remains_visible_and_active_when_question_mark_is_pressed() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    core.modal = Some(patch_modal(diff_preview("alpha\n", "beta\n")));

    terminal
        .draw(|frame| core.render(frame))
        .expect("draw before");
    assert!(terminal
        .backend()
        .screen_contents()
        .contains("Approval required"));

    assert_eq!(
        core.handle_input(key(KeyCode::Char('?'))),
        CoreEffect::Render
    );
    assert!(matches!(core.modal, Some(Modal::PatchApproval(_))));
    assert_eq!(core.bottom.composer().submit_text(), "?");

    terminal
        .draw(|frame| core.render(frame))
        .expect("draw after");
    let contents = terminal.backend().screen_contents();
    assert!(
        contents.contains("Approval required"),
        "contents:\n{contents}"
    );
    // Height-tight frames may clip the trailing hint line; the decision
    // keys are the durable affordance that must remain visible.
    assert!(
        contents.contains("y  Allow once") && contents.contains("n/esc  Deny"),
        "contents:\n{contents}"
    );
    assert!(!contents.contains("Euler keys"));
}

#[test]
fn interrupted_model_calls_do_not_duplicate_completed_tool_block() {
    // Dogfood session 01KWHP9X24HXKMVPKH9BC78QKK: a completed `ls` tool
    // result was followed by a model continuation the user interrupted,
    // then a second interrupted model call. Canonical events held exactly
    // one tool.result, but the visible transcript showed the tool block
    // twice. Interruption itself must not re-emit finalized history.
    let mut terminal = crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 12), 12)
        .expect("inline terminal");
    let mut core = core();
    render_compact_frame(&mut terminal, &mut core);
    let AppState::Idle { session } = std::mem::replace(&mut core.state, AppState::Empty) else {
        panic!("core should start idle");
    };
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    let ls_output = (1..=14)
        .map(|n| format!("entry-{n:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::TOOL_CALL,
            object([
                ("id", "call-ls".into()),
                ("name", "run_shell".into()),
                ("input", serde_json::json!({"command": "ls"})),
            ]),
        ),
    );
    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::TOOL_RESULT,
            object([
                ("id", "call-ls".into()),
                ("name", "run_shell".into()),
                ("ok", true.into()),
                ("exit_code", 0.into()),
                ("output", ls_output.into()),
            ]),
        ),
    );
    // Continuation model call that the user interrupts (dangling: no
    // model.result will arrive).
    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::MODEL_CALL,
            object([("provider", "fixture".into()), ("model", "echo".into())]),
        ),
    );
    core.handle_interrupt();
    render_compact_frame(&mut terminal, &mut core);
    core.handle_turn_event(TurnEvent::TurnDone {
        outcome: TurnOutcome::Cancelled,
        session,
    });
    render_compact_frame(&mut terminal, &mut core);

    // Second turn: another model call, interrupted again.
    let AppState::Idle { session } = std::mem::replace(&mut core.state, AppState::Empty) else {
        panic!("core should be idle after cancelled turn");
    };
    let (_tx2, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::USER_MESSAGE,
            object([("content", "you were stuck".into())]),
        ),
    );
    insert_event_and_render(
        &mut terminal,
        &mut core,
        event(
            EventKind::MODEL_CALL,
            object([("provider", "fixture".into()), ("model", "echo".into())]),
        ),
    );
    core.handle_interrupt();
    render_compact_frame(&mut terminal, &mut core);
    core.handle_turn_event(TurnEvent::TurnDone {
        outcome: TurnOutcome::Cancelled,
        session,
    });
    render_compact_frame(&mut terminal, &mut core);

    // Ownership discriminator: AppCore finalized projection must hold the
    // tool block exactly once. Two finalized copies means the bug is
    // transcript/visual projection; one copy here plus two in scrollback
    // means terminal commit/watermark behavior.
    let finalized = drain_finalized_visual_text(&mut core, 80);
    assert_eq!(
        finalized.matches("entry-01").count(),
        1,
        "finalized projection duplicated tool block: {finalized}"
    );
    assert_eq!(
        finalized
            .matches("interrupted — tell euler what to do differently")
            .count(),
        2,
        "two interruptions were finalized: {finalized}"
    );

    // Durable visible output: native scrollback plus the live screen
    // (scrollback_rows already appends the current screen rows).
    let durable = terminal.backend().scrollback_rows();
    let tool_footer_count = durable
        .iter()
        .filter(|row| row.contains("exit 0 · 14 lines"))
        .count();
    let first_entry_count = durable
        .iter()
        .filter(|row| row.contains("entry-01"))
        .count();
    assert_eq!(
        tool_footer_count, 1,
        "tool footer duplicated in durable output: {durable:?}"
    );
    assert_eq!(
        first_entry_count, 1,
        "tool output duplicated in durable output: {durable:?}"
    );
    let interrupted_count = durable
        .iter()
        .filter(|row| row.contains("interrupted — tell euler what to do differently"))
        .count();
    assert_eq!(
        interrupted_count, 2,
        "expected exactly the two finalized interruption notices: {durable:?}"
    );
    // The tool block must not repeat between the two notices.
    let first_notice = durable
        .iter()
        .position(|row| row.contains("interrupted — tell euler what to do differently"))
        .expect("first notice");
    assert!(
        !durable[first_notice..]
            .iter()
            .any(|row| row.contains("exit 0 · 14 lines")),
        "tool footer reappears after first interruption notice: {durable:?}"
    );
}

#[test]
fn turn_end_recap_follows_worked_duration_with_files_and_ctx() {
    let mut core = core();
    core.drain_finalized_visual_lines(72);
    core.turn_event_start = 0;
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::FILE_DIFF,
        object([
            ("path", "src/lib.rs".into()),
            ("diff", "--- a\n+++ b\n@@\n-old\n+new\n+extra\n".into()),
        ]),
    )));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "edited".into())]),
    )));
    core.handle_turn_outcome(TurnOutcome::Complete, Some(Duration::from_secs(8)));
    let text = drain_finalized_visual_text(&mut core, 72);
    assert!(text.contains("Worked for 8s"), "{text}");
    assert!(text.contains("1 file · +2 −1 · ctx ?%"), "{text}");
    assert!(text.contains("src/lib.rs"), "{text}");
}

#[test]
fn notifications_only_queue_when_unfocused_and_enabled() {
    let mut core = core();
    core.queue_notification(super::super::notify::NotifyEvent::TurnDone);
    assert!(core.take_pending_notification().is_none());
    core.set_terminal_focused(false);
    core.queue_notification(super::super::notify::NotifyEvent::TurnDone);
    assert_eq!(
        core.take_pending_notification(),
        Some(super::super::notify::NotifyEvent::TurnDone)
    );
    core.notifications_enabled = false;
    core.queue_notification(super::super::notify::NotifyEvent::Failure);
    assert!(core.take_pending_notification().is_none());
}

#[test]
fn exit_recap_is_bounded_and_copy_ready() {
    let core = core();
    let lines = core.exit_recap_lines();
    assert!(lines.len() <= 5);
    assert!(lines[1].text().contains("euler --resume"));
    assert!(lines[2].is_faint());
}
