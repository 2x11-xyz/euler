use super::*;

#[test]
fn permission_prompt_renders_inline_with_command_body() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    core.transcript.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "Model chatg ".repeat(60).into())]),
    ));
    core.transcript.push_event(event(
        EventKind::TOOL_CALL,
        object([
            ("id", "call-shell".into()),
            ("name", "run_shell".into()),
            (
                "input",
                serde_json::json!({"command": "bash -lc 'cargo test'"}),
            ),
        ]),
    ));
    core.modal = Some(Modal::Permission(PermissionRequest {
        capability: Capability::ShellExec,
        reason: "tool run_shell".to_owned(),
        command: None,
        path: None,
    }));

    terminal.draw(|frame| core.render(frame)).expect("draw");

    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("Would you like to run the following command?"));
    assert!(contents.contains("Reason: shell-exec: tool run_shell"));
    assert!(contents.contains("$ cargo test"));
    assert!(contents.contains("1. Yes, proceed (y)"));
    assert!(contents.contains("2. Yes, and don't ask again for shell-exec this session (a)"));
    assert!(contents.contains("3. No, and tell euler what to do differently (esc)"));
    assert!(!contents.contains("commands that start"));
    assert!(contents.contains("▌"));
    assert!(contents.contains("fixture/echo medium"));
    assert!(contents.contains("Context ?% used"));
    assert!(!contents.contains("◦ Working"));

    let rows = terminal.backend().screen_rows();
    assert!(
        rows[row_containing(&rows, "Would you like to run the following command?")]
            .starts_with("  "),
        "permission question should use content gutter: {rows:?}"
    );
    assert!(
        rows[row_containing(&rows, "1. Yes, proceed (y)")].starts_with("  "),
        "permission options should use content gutter: {rows:?}"
    );
}

#[test]
fn permission_prompt_uses_newest_run_shell_despite_later_non_shell_call() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    core.transcript.push_event(event(
        EventKind::TOOL_CALL,
        object([
            ("id", "call-shell".into()),
            ("name", "run_shell".into()),
            (
                "input",
                serde_json::json!({"command": "bash -lc 'cargo test'"}),
            ),
        ]),
    ));
    core.transcript.push_event(event(
        EventKind::TOOL_CALL,
        object([
            ("id", "call-read".into()),
            ("name", "read_file".into()),
            ("input", serde_json::json!({"path": "Cargo.toml"})),
        ]),
    ));
    core.modal = Some(Modal::Permission(PermissionRequest {
        capability: Capability::ShellExec,
        reason: "tool run_shell".to_owned(),
        command: None,
        path: None,
    }));

    terminal.draw(|frame| core.render(frame)).expect("draw");

    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("Would you like to run the following command?"));
    assert!(contents.contains("$ cargo test"));

    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    core.modal = Some(Modal::Permission(PermissionRequest {
        capability: Capability::FsWrite,
        reason: "tool edit_file".to_owned(),
        command: None,
        path: None,
    }));
    terminal.draw(|frame| core.render(frame)).expect("draw");

    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("Reason: fs-write: tool edit_file"));
    assert!(!contents.contains("$ cargo test"));
    assert!(!contents.contains("Would you like to run the following command?"));
}

#[test]
fn non_patch_permission_uses_generic_inline_ask() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    let request = PermissionRequest {
        capability: Capability::ShellExec,
        reason: "tool run_shell".to_owned(),
        command: None,
        path: None,
    };
    core.modal = Some(core.modal_for_request(request));

    terminal.draw(|frame| core.render(frame)).expect("draw");

    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("Would you like to allow this request?"));
    assert!(contents.contains("2. Yes, and don't ask again for shell-exec this session (a)"));
    assert!(!contents.contains("Patch approval required"));
}

#[test]
fn inline_permission_ask_keeps_all_options_visible_on_short_terminal() {
    let mut terminal = Terminal::new(VT100Backend::new(58, 11)).expect("terminal");
    let mut core = core();
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.modal = Some(Modal::Permission(PermissionRequest {
        capability: Capability::ShellExec,
        reason: "tool run_shell".to_owned(),
        command: None,
        path: None,
    }));

    terminal.draw(|frame| core.render(frame)).expect("draw");
    let rows = terminal.backend().screen_rows();
    assert!(
        rows[row_containing(&rows, "Would you like to allow this request?")].starts_with("  "),
        "permission prompt should align with transcript content gutter: {rows:?}"
    );
    let one = row_containing(&rows, "1. Yes, proceed (y)");
    let two = row_containing(&rows, "2. Yes, and don't ask again");
    let three = row_containing(&rows, "3. No, and tell euler");
    let prompt = row_containing(&rows, "▌");
    let status = row_containing(&rows, "fixture/echo");
    assert!(one < two && two < three, "rows: {rows:?}");
    assert!(three < prompt, "rows: {rows:?}");
    assert_eq!(
        status,
        prompt + 1,
        "short viewport should prioritize permission options over footer spacer, rows: {rows:?}"
    );

    assert_eq!(core.handle_input(key(KeyCode::Esc)), CoreEffect::Render);
    assert_eq!(reply_rx.recv().expect("reply"), PermissionReply::Deny);
}

#[test]
fn inline_terminal_permission_ask_keeps_options_visible_in_constrained_viewport() {
    let mut terminal = crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 8), 8)
        .expect("inline terminal");
    let mut core = core();
    core.notice = Some("lower priority notice".to_owned());
    core.transcript.push_event(event(
        EventKind::MODEL_DELTA,
        object([
            ("kind", "text".into()),
            ("delta", "live transcript should yield\n".into()),
        ]),
    ));
    core.modal = Some(Modal::Permission(PermissionRequest {
        capability: Capability::FsWrite,
        reason: "tool edit_file".to_owned(),
        command: None,
        path: None,
    }));

    render_inline_frame(&mut terminal, &mut core);

    let rows = terminal.backend().screen_rows();
    let one = row_containing(&rows, "1. Yes, proceed (y)");
    let two = row_containing(&rows, "2. Yes, and don't ask again");
    let three = row_containing(&rows, "3. No, and tell euler");
    let prompt = row_containing(&rows, "▌");
    let status = row_containing(&rows, "fixture/echo");
    assert_eq!(terminal.viewport_area().height, 8);
    assert!(one < two && two < three, "rows: {rows:?}");
    assert!(three < prompt, "rows: {rows:?}");
    assert_footer_breathing_room(&rows, prompt, status);
    assert!(
        !rows.iter().any(|row| row.contains("lower priority notice")),
        "notice should yield before permission options clip, rows: {rows:?}"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.contains("live transcript should yield")),
        "transcript should yield before permission options clip, rows: {rows:?}"
    );
}

#[test]
fn inline_patch_approval_ask_hides_working_status_and_keeps_options_visible() {
    let mut terminal = crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 12), 12)
        .expect("inline terminal");
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    core.modal = Some(patch_modal(diff_preview("alpha\n", "beta\n")));

    render_inline_frame(&mut terminal, &mut core);

    let rows = terminal.backend().screen_rows();
    let one = row_containing(&rows, "1. Yes, proceed (y)");
    let two = row_containing(&rows, "2. Yes, and don't ask again");
    let three = row_containing(&rows, "3. No, and tell euler");
    let prompt = row_containing(&rows, "▌");
    let status = row_containing(&rows, "fixture/echo");
    assert!(one < two && two < three, "rows: {rows:?}");
    assert!(three < prompt, "rows: {rows:?}");
    assert_footer_breathing_room(&rows, prompt, status);
    assert!(
        !rows.iter().any(|row| row.contains("◦ Working")),
        "patch approval should own the live flow, rows: {rows:?}"
    );
}

#[test]
fn permission_inline_ask_esc_denies_and_restores_composer_status() {
    let width = 48;
    let height = 16;
    let mut terminal = Terminal::new(VT100Backend::new(width, height)).expect("terminal");
    let mut core = core();
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.transcript.push_event(event(
        EventKind::MODEL_DELTA,
        object([
            ("kind", "text".into()),
            ("delta", "underlying transcript\n".into()),
        ]),
    ));
    core.bottom.composer_mut().insert_text("draft");
    core.modal = Some(Modal::Permission(PermissionRequest {
        capability: Capability::ShellExec,
        reason: "run command".to_owned(),
        command: None,
        path: None,
    }));

    terminal.draw(|frame| core.render(frame)).expect("draw");

    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("Would you like"));

    assert_eq!(core.handle_input(key(KeyCode::Esc)), CoreEffect::Render);
    assert_eq!(reply_rx.recv().expect("reply"), PermissionReply::Deny);
    terminal.draw(|frame| core.render(frame)).expect("redraw");

    let restored = terminal.backend().screen_contents();
    assert!(!restored.contains("Would you like"));
    assert!(restored.contains("underlying transcript"));
    assert!(restored.contains("▌ draft"));
    assert!(restored.contains("fixture/echo medium"));
    assert!(restored.contains("Context ?% used"));
}

fn row_containing(rows: &[String], needle: &str) -> usize {
    rows.iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("expected row containing {needle:?}, rows: {rows:?}"))
}

fn assert_footer_breathing_room(rows: &[String], prompt: usize, status: usize) {
    assert_eq!(status, prompt + 2, "rows: {rows:?}");
    assert!(rows[prompt + 1].trim().is_empty(), "rows: {rows:?}");
}

fn render_inline_frame(
    terminal: &mut crate::ui::terminal::InlineTerminal<VT100Backend>,
    core: &mut AppCore,
) {
    let width = terminal.size().expect("terminal size").width;
    let frame = core.render_visual_canvas(width);
    terminal
        .draw_visual_frame(&frame)
        .expect("draw visual frame");
}
