use super::{
    test_backend::VT100Backend,
    text::display_width,
    theme::Theme,
    transcript::{
        normalized_shell_command, project_events, project_latest_event_for_ui,
        render_items_for_history, render_items_for_history_with_limit, render_line_oriented,
        transcript_widget, TranscriptItem,
    },
};
use euler_event::{object, EventEnvelope, EventKind};
use ratatui::{layout::Rect, style::Style, text::Line, Terminal};

use super::transcript::TranscriptState;

const DEFAULT_OUTPUT_LIMIT_LINES: usize = super::transcript::TOOL_CALL_MAX_LINES;

#[test]
fn projects_supported_events_and_skips_control_events() {
    let events = vec![
        event(EventKind::SESSION_START, object([])),
        event(
            EventKind::USER_MESSAGE,
            object([("content", "hello".into())]),
        ),
        event(
            EventKind::MODEL_CALL,
            object([("provider", "fixture".into()), ("model", "echo".into())]),
        ),
        event(
            EventKind::MODEL_RESULT,
            object([("content", "reply".into())]),
        ),
        event(
            EventKind::ASSISTANT_MESSAGE,
            object([("content", "reply".into())]),
        ),
    ];

    assert_eq!(
        project_events(&events),
        vec![
            TranscriptItem::UserMessage("hello".to_owned()),
            TranscriptItem::ModelCall {
                provider: "fixture".to_owned(),
                model: "echo".to_owned(),
            },
            TranscriptItem::ModelResult("reply".to_owned()),
            TranscriptItem::AssistantMessage("reply".to_owned()),
        ]
    );
}

#[test]
fn transcript_state_streams_live_tail_then_finalizes_without_duplicate() {
    let mut state = TranscriptState::default();
    state.push_event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "hel".into())]),
    ));
    state.push_event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "lo".into())]),
    ));

    assert_eq!(
        state.items(),
        vec![TranscriptItem::AssistantMessage("hello".to_owned())]
    );
    state.push_event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "\n".into())]),
    ));

    assert_eq!(
        state.items(),
        vec![TranscriptItem::AssistantMessage("hello\n".to_owned())]
    );

    state.push_event(event(
        EventKind::MODEL_RESULT,
        object([("content", "hello".into())]),
    ));
    state.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "hello".into())]),
    ));

    assert_eq!(
        state.items(),
        vec![TranscriptItem::AssistantMessage("hello".to_owned())]
    );
}

#[test]
fn transcript_state_preserves_live_tail_across_tool_call_model_result() {
    let mut state = TranscriptState::default();
    state.push_event(event(
        EventKind::MODEL_DELTA,
        object([
            ("kind", "text".into()),
            ("delta", "I will inspect it.\n".into()),
        ]),
    ));

    state.push_event(event(
        EventKind::MODEL_RESULT,
        object([
            ("content", "I will inspect it.\n".into()),
            (
                "tool_calls",
                serde_json::json!([
                    {
                        "id": "call-read",
                        "name": "read_file",
                        "input": {"path": "Cargo.toml"}
                    }
                ]),
            ),
        ]),
    ));

    assert_eq!(
        state.live_items(),
        vec![TranscriptItem::AssistantMessage(
            "I will inspect it.\n".to_owned()
        )]
    );
    assert_eq!(
        state.items(),
        vec![TranscriptItem::AssistantMessage(
            "I will inspect it.\n".to_owned()
        )]
    );
}

#[test]
fn transcript_state_preserves_uncommitted_live_tail_across_tool_call_model_result() {
    let mut state = TranscriptState::default();
    state.push_event(event(
        EventKind::MODEL_DELTA,
        object([
            ("kind", "text".into()),
            ("delta", "I will inspect it.".into()),
        ]),
    ));

    state.push_event(event(
        EventKind::MODEL_RESULT,
        object([
            ("content", "I will inspect it.".into()),
            (
                "tool_calls",
                serde_json::json!([
                    {
                        "id": "call-read",
                        "name": "read_file",
                        "input": {"path": "Cargo.toml"}
                    }
                ]),
            ),
        ]),
    ));

    assert_eq!(
        state.live_items(),
        vec![TranscriptItem::AssistantMessage(
            "I will inspect it.".to_owned()
        )]
    );
    assert_eq!(
        state.items(),
        vec![TranscriptItem::AssistantMessage(
            "I will inspect it.".to_owned()
        )]
    );
}

#[test]
fn transcript_state_no_newline_delta_previews_then_finalizes_without_duplicate() {
    let mut state = TranscriptState::default();
    state.push_event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "single line".into())]),
    ));

    assert_eq!(
        state.live_items(),
        vec![TranscriptItem::AssistantMessage("single line".to_owned())]
    );

    state.push_event(event(
        EventKind::MODEL_RESULT,
        object([("content", "single line".into())]),
    ));

    assert_eq!(state.live_items(), Vec::<TranscriptItem>::new());
    assert_eq!(
        state.items(),
        vec![TranscriptItem::AssistantMessage("single line".to_owned())]
    );
    let rendered = line_texts(&render_items_for_history(
        &state.items(),
        &Theme::default(),
        80,
    ))
    .join("\n");
    assert_eq!(rendered.matches("single line").count(), 1);

    state.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "single line".into())]),
    ));

    assert_eq!(state.live_items(), Vec::<TranscriptItem>::new());
    assert_eq!(
        state.items(),
        vec![TranscriptItem::AssistantMessage("single line".to_owned())]
    );
    assert_eq!(
        project_events(state.events()),
        vec![
            TranscriptItem::ModelResult("single line".to_owned()),
            TranscriptItem::AssistantMessage("single line".to_owned()),
        ]
    );
}

#[test]
fn transcript_state_streams_table_frame_then_finalizes_without_trailing_blank() {
    let table = "| A | B |\n|---|---|\n| 1 | 2 |";
    let mut state = TranscriptState::default();
    state.push_event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", table.into())]),
    ));

    assert_eq!(
        state.live_items(),
        vec![TranscriptItem::AssistantMessage(
            "| A | B |\n|---|---|\n".to_owned()
        )]
    );
    state.push_event(event(
        EventKind::MODEL_RESULT,
        object([("content", table.into())]),
    ));

    assert_eq!(state.live_items(), Vec::<TranscriptItem>::new());
    assert_eq!(
        state.items(),
        vec![TranscriptItem::AssistantMessage(table.to_owned())]
    );

    state.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", table.into())]),
    ));

    assert_eq!(
        state.items(),
        vec![TranscriptItem::AssistantMessage(table.to_owned())]
    );
}

#[test]
fn transcript_model_result_fallback_dedupes_only_same_content_assistant_message() {
    let mut different = TranscriptState::default();
    different.push_event(event(
        EventKind::MODEL_RESULT,
        object([("content", "A".into())]),
    ));
    different.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "B".into())]),
    ));

    assert_eq!(
        different.items(),
        vec![
            TranscriptItem::AssistantMessage("A".to_owned()),
            TranscriptItem::AssistantMessage("B".to_owned()),
        ]
    );

    let mut same = TranscriptState::default();
    same.push_event(event(
        EventKind::MODEL_RESULT,
        object([("content", "A".into())]),
    ));
    same.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "A".into())]),
    ));

    assert_eq!(
        same.items(),
        vec![TranscriptItem::AssistantMessage("A".to_owned())]
    );
}

#[test]
fn transcript_model_result_same_content_then_followup_assistant_keeps_order() {
    let mut state = TranscriptState::default();
    state.push_event(event(
        EventKind::MODEL_RESULT,
        object([("content", "A".into())]),
    ));
    assert_eq!(
        project_latest_event_for_ui(state.events()),
        Some(TranscriptItem::AssistantMessage("A".to_owned()))
    );

    state.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "A".into())]),
    ));
    assert_eq!(project_latest_event_for_ui(state.events()), None);

    state.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "B".into())]),
    ));
    assert_eq!(
        project_latest_event_for_ui(state.events()),
        Some(TranscriptItem::AssistantMessage("B".to_owned()))
    );

    assert_eq!(
        state.items(),
        vec![
            TranscriptItem::AssistantMessage("A".to_owned()),
            TranscriptItem::AssistantMessage("B".to_owned()),
        ]
    );
}

#[test]
fn transcript_model_result_intervening_assistant_owns_later_same_content_order() {
    let mut state = TranscriptState::default();
    state.push_event(event(
        EventKind::MODEL_RESULT,
        object([("content", "A".into())]),
    ));
    state.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "B".into())]),
    ));
    state.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "A".into())]),
    ));

    // The fallback belongs before the first assistant owner. Once B arrives,
    // the later same-content A is a separate assistant message, not the owner
    // of the earlier model.result fallback.
    assert_eq!(
        state.items(),
        vec![
            TranscriptItem::AssistantMessage("A".to_owned()),
            TranscriptItem::AssistantMessage("B".to_owned()),
            TranscriptItem::AssistantMessage("A".to_owned()),
        ]
    );
}

#[test]
fn tui_history_suppresses_model_lifecycle_and_shows_final_answer_once() {
    let events = vec![
        event(
            EventKind::MODEL_CALL,
            object([("provider", "fixture".into()), ("model", "echo".into())]),
        ),
        event(
            EventKind::MODEL_RESULT,
            object([("content", "final answer".into())]),
        ),
        event(
            EventKind::ASSISTANT_MESSAGE,
            object([("content", "final answer".into())]),
        ),
    ];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 80, 6);

    assert!(!contents.contains("* Model fixture/echo"));
    assert!(!contents.contains("* Model result"));
    assert_eq!(contents.matches("final answer").count(), 1);
}

#[test]
fn tui_items_insert_turn_separator_between_turns_not_inside_markdown_answer() {
    let mut state = TranscriptState::default();
    state.push_event(event(
        EventKind::USER_MESSAGE,
        object([("content", "first".into())]),
    ));
    state.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "alpha\n\nbeta".into())]),
    ));
    state.push_event(event(
        EventKind::USER_MESSAGE,
        object([("content", "second".into())]),
    ));
    let items = state.items();
    assert!(items.contains(&TranscriptItem::TurnSeparator));

    let theme = Theme::default();
    let contents = line_texts(&render_items_for_history(&items, &theme, 40)).join("\n");
    // Turn separator is one full-width rule; each meaningful block also has a
    // dim hairline, so total ─ rows > 1. The turn separator itself is the only
    // non-gutter-prefixed ─ row.
    let turn_rules = contents
        .lines()
        .filter(|line| line.contains('─') && !line.starts_with("         "))
        .count();
    assert_eq!(turn_rules, 1, "contents: {contents:?}");
    assert!(contents.contains("alpha"));
    assert!(contents.contains("beta"));
}

#[test]
fn tui_history_suppresses_routine_allow_permission_rows() {
    let events = vec![
        event(
            EventKind::PERMISSION_DECISION,
            object([
                ("capability", "fs-read".into()),
                ("decision", "allowed".into()),
                ("allowed", true.into()),
            ]),
        ),
        event(
            EventKind::PERMISSION_DECISION,
            object([
                ("capability", "shell-exec".into()),
                ("decision", "denied".into()),
                ("allowed", false.into()),
            ]),
        ),
        event(
            EventKind::PERMISSION_DECISION,
            object([
                ("capability", "artifact-write".into()),
                ("mode", "static-grant".into()),
                ("decision", "allowed".into()),
                ("allowed", true.into()),
                ("source", "extension".into()),
                ("extension_id", "causal-dag".into()),
                ("command", serde_json::Value::Null),
            ]),
        ),
        event(
            EventKind::PERMISSION_DECISION,
            object([
                ("capability", "network".into()),
                ("mode", "static-grant".into()),
                ("decision", "denied".into()),
                ("allowed", false.into()),
                ("source", "extension".into()),
                ("extension_id", "causal-dag".into()),
                ("command", "network-check".into()),
            ]),
        ),
    ];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 80, 6);

    assert!(!contents.contains("Permission approved: fs-read"));
    assert!(!contents.contains("Permission approved: artifact-write"));
    assert!(contents.contains("✗ Permission denied: shell-exec (denied)"));
    assert!(contents.contains("✗ Permission denied: network (denied)"));
}

#[test]
fn tui_permission_decisions_render_approved_and_canceled_notices() {
    let events = vec![
        event(
            EventKind::PERMISSION_DECISION,
            object([
                ("capability", "shell-exec".into()),
                ("decision", "allowed".into()),
                ("allowed", true.into()),
            ]),
        ),
        event(
            EventKind::PERMISSION_DECISION,
            object([
                ("capability", "fs-write".into()),
                ("decision", "canceled".into()),
                ("allowed", false.into()),
            ]),
        ),
    ];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 80, 6);

    assert!(contents.contains("✓ Permission approved: shell-exec (allowed)"));
    assert!(contents.contains("✗ Permission canceled: fs-write (canceled)"));
}

#[test]
fn tui_read_tool_flow_uses_compact_result_without_raw_lifecycle_rows() {
    let events = vec![
        event(
            EventKind::TOOL_CALL,
            object([
                ("id", "call-read".into()),
                ("name", "read_file".into()),
                ("input", serde_json::json!({"path": "README.md"})),
            ]),
        ),
        event(
            EventKind::PERMISSION_DECISION,
            object([
                ("capability", "fs-read".into()),
                ("decision", "allowed".into()),
                ("allowed", true.into()),
            ]),
        ),
        event(
            EventKind::TOOL_RESULT,
            object([
                ("id", "call-read".into()),
                ("name", "read_file".into()),
                ("ok", true.into()),
                (
                    "output",
                    "raw file contents that should stay out of history".into(),
                ),
            ]),
        ),
    ];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 80, 6);

    assert!(contents.contains("explore"));
    assert!(contents.contains("Read README.md"));
    assert!(!contents.contains("raw file contents"));
    assert!(!contents.contains("* Tool read_file"));
    assert!(!contents.contains("read_file call"));
    assert!(!contents.contains("Tool read_file completed"));
    assert!(!contents.contains("Permission allowed"));
}

#[test]
fn tui_shell_run_uses_raw_command_label_without_semantic_prefix() {
    let events = vec![
        tool_call(
            "call-rg",
            "run_shell",
            serde_json::json!({"command": "bash -lc \"rg transcript crates/euler-cli/src/ui\""}),
        ),
        tool_result("call-rg", "run_shell", "transcript.rs:match"),
    ];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 96, 8);

    assert!(contents.contains("bash $ rg transcript crates/euler-cli/src/ui"));
    assert!(!contents.contains("• Ran Search"));
    assert!(!contents.contains("Search rg transcript"));
    assert!(contents.contains("  transcript.rs:match"));
}

#[test]
fn shell_label_normalization_strips_bash_lc_and_caps_display_width() {
    let command = format!("bash -lc \"{}\"", "a".repeat(120));

    let normalized = normalized_shell_command(&command);

    assert!(!normalized.contains("bash -lc"));
    assert!(normalized.ends_with('…'));
    assert_eq!(display_width(&normalized), 80);
}

#[test]
fn shell_label_normalization_summarizes_multiline_commands() {
    let command = concat!(
        "bash -lc \"\n",
        "\tmkdir -p /tmp/euler-dogfood-file-change \\\n",
        "cd /tmp/euler-dogfood-file-change\n",
        "cat > hello.rs <<'EOF'\""
    );

    let normalized = normalized_shell_command(command);

    assert!(normalized.starts_with("mkdir -p /tmp/euler-dogfood-file-change"));
    assert!(normalized.contains("(+2 lines)"));
    assert!(!normalized.contains("cd /tmp"));
    assert!(!normalized.contains('\n'));
    assert!(!normalized.contains('\r'));
    assert!(display_width(&normalized) <= 80);
}

#[test]
fn shell_label_normalization_reserves_width_for_multiline_suffix() {
    let command = format!("{}\necho done", "a".repeat(120));

    let normalized = normalized_shell_command(&command);

    assert!(normalized.ends_with(" … (+1 lines)"));
    assert_eq!(display_width(&normalized), 80);
}

#[test]
fn shell_label_normalization_sanitizes_controls_and_tabs() {
    let command = "\u{1b}[31m\tcargo test\u{7}\nnext";

    let normalized = normalized_shell_command(command);

    assert_eq!(normalized, "cargo test … (+1 lines)");
    assert!(!normalized.contains('\u{1b}'));
    assert!(!normalized.contains('\t'));
}

#[test]
fn shell_label_normalization_strips_string_escape_sequences() {
    let command = "\u{1b}]0;bad title\u{7}cargo test\n\u{1b}Pignore me\u{1b}\\next";

    let normalized = normalized_shell_command(command);

    assert_eq!(normalized, "cargo test … (+1 lines)");
    assert!(!normalized.contains("bad title"));
    assert!(!normalized.contains("ignore me"));
    assert!(!normalized.contains('\u{1b}'));
}

#[test]
fn shell_label_normalization_strips_unsupported_escape_forms() {
    let command = "\u{1b}(Bcargo test\n\u{1b}cnext";

    let normalized = normalized_shell_command(command);

    assert_eq!(normalized, "cargo test … (+1 lines)");
    assert!(!normalized.contains('\u{1b}'));
}

#[test]
fn shell_label_normalization_reserves_suffix_width_for_wide_chars() {
    let command = format!("echo {}\nnext", "\u{754c}".repeat(80));

    let normalized = normalized_shell_command(&command);

    assert!(normalized.ends_with(" … (+1 lines)"));
    assert!(display_width(&normalized) <= 80);
}

#[test]
fn shell_label_normalization_keeps_blank_commands_empty() {
    let normalized = normalized_shell_command("\n \r\n\t");

    assert_eq!(normalized, "");
}

#[test]
fn tui_exploration_coalesces_and_dedupes_read_labels() {
    let events = vec![
        tool_call(
            "call-readme",
            "read_file",
            serde_json::json!({"path": "README.md"}),
        ),
        tool_result(
            "call-readme",
            "read_file",
            "README raw content should not render",
        ),
        tool_call(
            "call-cargo",
            "read_file",
            serde_json::json!({"path": "Cargo.toml"}),
        ),
        tool_result(
            "call-cargo",
            "read_file",
            "Cargo raw content should not render",
        ),
        tool_call(
            "call-readme-again",
            "read_file",
            serde_json::json!({"path": "README.md"}),
        ),
        tool_result(
            "call-readme-again",
            "read_file",
            "duplicate README raw content should not render",
        ),
        tool_call(
            "call-rg",
            "run_shell",
            serde_json::json!({"command": "rg transcript crates/euler-cli/src/ui"}),
        ),
        tool_result("call-rg", "run_shell", "exit 0\ntranscript.rs:match"),
    ];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 96, 10);

    assert_eq!(contents.matches("explore").count(), 1);
    assert!(contents.contains("└ Read README.md, Cargo.toml"));
    assert!(contents.contains("bash $ rg transcript crates/euler-cli/src/ui"));
    assert!(!contents.contains("Search rg transcript crates/euler-cli/src/ui"));
    assert!(!contents.contains("README raw content"));
    assert!(!contents.contains("Cargo raw content"));
    assert!(contents.contains("transcript.rs:match"));
}

#[test]
fn tui_assistant_finalization_does_not_leave_stale_exploration_fragments() {
    let events = vec![
        tool_call(
            "call-read",
            "read_file",
            serde_json::json!({"path": "AGENTS.md"}),
        ),
        tool_result("call-read", "read_file", "# raw agent instructions"),
        event(
            EventKind::MODEL_RESULT,
            object([("content", "final answer".into())]),
        ),
        event(
            EventKind::ASSISTANT_MESSAGE,
            object([("content", "final answer".into())]),
        ),
    ];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 80, 8);

    assert!(contents.contains("explore"));
    assert!(contents.contains("Read AGENTS.md"));
    assert_eq!(contents.matches("final answer").count(), 1);
    assert!(!contents.contains("read_file call"));
    assert!(!contents.contains("# raw agent instructions"));
}

#[test]
fn tui_failed_read_tool_keeps_diagnostic_output_visible() {
    let events = vec![
        tool_call(
            "call-missing",
            "read_file",
            serde_json::json!({"path": "missing.txt"}),
        ),
        event(
            EventKind::TOOL_RESULT,
            object([
                ("id", "call-missing".into()),
                ("name", "read_file".into()),
                ("ok", false.into()),
                ("error", "No such file or directory".into()),
                ("output", "os error detail".into()),
            ]),
        ),
    ];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 80, 6);

    assert!(contents.contains("explore ✗ No such file or directory"));
    assert!(contents.contains("os error detail"));
}

#[test]
fn tui_edit_flow_keeps_compact_patch_result_without_allow_spam() {
    let events = vec![
        event(EventKind::TOOL_CALL, object([("name", "edit_file".into())])),
        event(
            EventKind::PERMISSION_PROMPT,
            object([
                ("capability", "fs-write".into()),
                ("reason", "tool edit_file".into()),
            ]),
        ),
        event(
            EventKind::PERMISSION_DECISION,
            object([
                ("capability", "fs-write".into()),
                ("decision", "allowed".into()),
                ("allowed", true.into()),
            ]),
        ),
        event(
            EventKind::PATCH_PROPOSED,
            object([
                ("path", "src/lib.rs".into()),
                ("old", "one\n".into()),
                ("new", "one\ntwo\n".into()),
            ]),
        ),
        event(
            EventKind::PATCH_APPLIED,
            object([
                ("path", "src/lib.rs".into()),
                ("old", "one\n".into()),
                ("new", "one\ntwo\n".into()),
            ]),
        ),
        event(
            EventKind::TOOL_RESULT,
            object([
                ("name", "edit_file".into()),
                ("ok", true.into()),
                ("output", "edited src/lib.rs".into()),
            ]),
        ),
    ];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 80, 14);

    assert!(contents.contains("edit src/lib.rs · +1 −0"));
    assert!(contents.contains("two"));
    assert!(!contents.contains("Permission required"));
    assert!(!contents.contains("Permission allowed"));
    assert!(!contents.contains("• Patch proposed"));
    assert!(!contents.contains("Tool edit_file"));
    assert!(!contents.contains("Tool edit_file"));
}

#[test]
fn transcript_state_clears_live_tail_on_error_before_later_turn() {
    let mut state = TranscriptState::default();
    state.push_event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "partial".into())]),
    ));
    state.push_event(event(
        EventKind::ERROR,
        object([("source", "provider".into()), ("message", "failed".into())]),
    ));

    assert!(!state
        .items()
        .contains(&TranscriptItem::AssistantMessage("partial".to_owned())));

    state.push_event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "fresh\n".into())]),
    ));

    assert!(state
        .items()
        .contains(&TranscriptItem::AssistantMessage("fresh\n".to_owned())));
    assert!(!state
        .items()
        .contains(&TranscriptItem::AssistantMessage("partialfresh".to_owned())));
}

#[test]
fn transcript_state_clear_transient_live_tail_handles_cancel_before_later_turn() {
    let mut state = TranscriptState::default();
    state.push_event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "cancelled".into())]),
    ));

    state.clear_transient_live_tail();
    state.push_event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "next\n".into())]),
    ));

    assert_eq!(
        state.items(),
        vec![TranscriptItem::AssistantMessage("next\n".to_owned())]
    );
}

#[test]
fn line_oriented_renderer_preserves_existing_labels() {
    let events = vec![
        event(EventKind::TOOL_CALL, object([("name", "read_file".into())])),
        event(
            EventKind::TOOL_RESULT,
            object([
                ("name", "read_file".into()),
                ("ok", true.into()),
                ("output", "contents".into()),
            ]),
        ),
        event(
            EventKind::TOOL_RESULT,
            object([
                ("name", "run_shell".into()),
                ("ok", false.into()),
                ("error", "denied".into()),
            ]),
        ),
        event(
            EventKind::PERMISSION_PROMPT,
            object([("capability", "shell-exec".into())]),
        ),
        event(
            EventKind::PERMISSION_DECISION,
            object([("decision", "denied".into())]),
        ),
        event(
            EventKind::PATCH_PROPOSED,
            object([
                ("path", "src/lib.rs".into()),
                ("old", "one\n".into()),
                ("new", "one\ntwo\n".into()),
            ]),
        ),
        event(
            EventKind::PATCH_APPLIED,
            object([
                ("path", "src/lib.rs".into()),
                ("old", "one\n".into()),
                ("new", "".into()),
            ]),
        ),
    ];

    assert_eq!(
        render_line_oriented(&events),
        "tool.call: read_file\n\
tool.result: read_file ok\n\
tool.result: run_shell failed: denied\n\
permission.prompt: shell-exec\n\
permission.decision: denied\n\
patch.proposed: update: src/lib.rs\n\
patch.applied: delete: src/lib.rs\n"
    );
}

#[test]
fn line_oriented_renderer_ignores_event_timestamps() {
    let events = vec![event_at(
        EventKind::USER_MESSAGE,
        object([("content", "timestamped".into())]),
        "2026-06-20T14:32:07.000Z",
    )];

    assert_eq!(render_line_oriented(&events), "user: timestamped\n");
}

#[test]
fn markdown_assistant_tui_projection_preserves_line_oriented_invariants() {
    let markdown = "**bold**\n\n- item";
    let events = vec![event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", markdown.into())]),
    )];
    let theme = Theme::default();

    assert_eq!(
        project_events(&events),
        vec![TranscriptItem::AssistantMessage(markdown.to_owned())]
    );
    assert_eq!(
        render_line_oriented(&events),
        "assistant: **bold**\n\n- item\n"
    );

    let contents = rendered_screen(&events, &theme, 80, 6);
    assert!(contents.contains("bold"));
    assert!(contents.contains("item"));
    assert!(!contents.contains("• bold"));
    assert!(contents.contains("- item"));
    assert!(!contents.contains("**bold**"));
}

#[test]
fn projects_slice2_events_without_opaque_reasoning_artifacts() {
    let events = vec![
        event(
            EventKind::PLAN_UPDATE,
            object([("summary", "inspect renderer".into())]),
        ),
        event(
            EventKind::MODEL_REASONING,
            object([
                ("fidelity", "summary".into()),
                ("content", "Checked the projection path.".into()),
            ]),
        ),
        event(
            EventKind::MODEL_REASONING,
            object([
                ("fidelity", "opaque".into()),
                ("content", "".into()),
                ("artifact", "provider-secret-artifact".into()),
            ]),
        ),
        event(
            EventKind::TOOL_RESULT,
            object([
                ("name", "run_shell".into()),
                ("ok", false.into()),
                ("error", "failed".into()),
                ("output", "partial output".into()),
                ("exit_code", 1.into()),
            ]),
        ),
        event(
            EventKind::PERMISSION_PROMPT,
            object([
                ("capability", "shell-exec".into()),
                ("reason", "run checks".into()),
            ]),
        ),
        event(
            EventKind::PERMISSION_DECISION,
            object([
                ("capability", "shell-exec".into()),
                ("decision", "session-deny".into()),
                ("allowed", false.into()),
            ]),
        ),
        event(
            EventKind::CHECK_STARTED,
            object([("name", "cargo test".into())]),
        ),
        event(
            EventKind::CHECK_RESULT,
            object([
                ("name", "cargo test".into()),
                ("ok", true.into()),
                ("output", "ok".into()),
            ]),
        ),
        event(
            EventKind::SESSION_SUMMARY,
            object([("summary", "done".into())]),
        ),
        event(
            EventKind::ERROR,
            object([
                ("source", "provider".into()),
                ("message", "rate limit".into()),
            ]),
        ),
    ];

    assert_eq!(
        project_events(&events),
        vec![
            TranscriptItem::PlanUpdate("inspect renderer".to_owned()),
            TranscriptItem::ModelReasoning {
                fidelity: "summary".to_owned(),
                content: "Checked the projection path.".to_owned(),
            },
            TranscriptItem::ToolResult {
                name: "run_shell".to_owned(),
                ok: false,
                error: "failed".to_owned(),
                output: "partial output".to_owned(),
                exit_code: Some(1),
                path: None,
            },
            TranscriptItem::PermissionPrompt {
                capability: "shell-exec".to_owned(),
                reason: "run checks".to_owned(),
            },
            TranscriptItem::PermissionDecision {
                capability: "shell-exec".to_owned(),
                decision: "session-deny".to_owned(),
                allowed: Some(false),
            },
            TranscriptItem::CheckStarted {
                name: "cargo test".to_owned(),
            },
            TranscriptItem::CheckResult {
                name: "cargo test".to_owned(),
                ok: true,
                output: "ok".to_owned(),
            },
            TranscriptItem::SessionSummary("done".to_owned()),
            TranscriptItem::Error {
                source: "provider".to_owned(),
                message: "rate limit".to_owned(),
            },
        ]
    );
}

#[test]
fn vt100_render_wraps_with_stable_gutter_and_bounded_output() {
    let events = vec![
        event(
            EventKind::USER_MESSAGE,
            object([("content", "alpha beta gamma delta epsilon".into())]),
        ),
        event(
            EventKind::TOOL_RESULT,
            object([
                ("name", "run_shell".into()),
                ("ok", true.into()),
                (
                    "output",
                    "line one output\nline two output\nline three output\nline four output\nline five output"
                        .into(),
                ),
            ]),
        ),
    ];
    let theme = Theme::default();
    let contents = rendered_screen_with_limit(&events, &theme, 40, 16, 2);
    assert!(contents.contains("▌ alpha beta gamma"));
    // Timestamp stamp or blank gutter may precede the rail; match the glyph.
    let prompt_glyph_lines = contents
        .lines()
        .filter(|line| line.contains("▌ "))
        .collect::<Vec<_>>();
    assert_eq!(
        prompt_glyph_lines.len(),
        2,
        "expected continuous rail for each wrapped user prompt row, got {prompt_glyph_lines:?}"
    );
    assert!(contents.contains("line one output"));
    assert!(contents.contains("line two output"));
    assert!(contents.contains("1 more lines"));
    assert!(contents.contains("line five output"));

    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        let trimmed = line.trim_start();
        assert!(
            line.starts_with("         ")
                || line.starts_with("       └ ")
                || line.starts_with("       ├ ")
                || line.contains("▌ ")
                || trimmed.starts_with("• ")
                || trimmed.starts_with("bash")
                || line.contains("─")
                || line.chars().next().is_some_and(|ch| ch.is_ascii_digit()),
            "unstable gutter: {line:?}"
        );
    }
}

#[test]
fn vt100_multiline_user_message_uses_continuous_rail_for_whole_block() {
    let events = vec![event(
        EventKind::USER_MESSAGE,
        object([("content", "one\ntwo three four five six seven".into())]),
    )];
    let theme = Theme::default();
    // Wider + taller so the 9-cell gutter, hairline, and turn footer leave room
    // for the full multi-line user block (not just the scrolled tail).
    let contents = rendered_screen_with_limit(&events, &theme, 28, 12, 2);
    let user_rows = contents
        .lines()
        .filter(|line| line.contains("▌ "))
        .collect::<Vec<_>>();

    assert!(
        user_rows
            .iter()
            .any(|line| line.contains("▌ one") || line.contains("▌ one ")),
        "rows: {user_rows:?} contents: {contents:?}"
    );
    assert!(
        user_rows.len() >= 3,
        "expected explicit newline and soft-wrap continuations to keep the rail: {contents:?}"
    );
}

#[test]
fn vt100_blank_multiline_user_message_keeps_continuous_rail() {
    let events = vec![event(
        EventKind::USER_MESSAGE,
        object([("content", "\n\n".into())]),
    )];
    let theme = Theme::default();
    let contents = rendered_screen_with_limit(&events, &theme, 18, 8, 2);
    let user_rows = contents
        .lines()
        .filter(|line| line.contains("▌ "))
        .collect::<Vec<_>>();

    assert_eq!(
        user_rows.len(),
        3,
        "expected one rail on each blank multiline row: {contents:?}"
    );
}

#[test]
fn vt100_long_tool_output_uses_head_tail_affordance() {
    let output = (1..=12)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let events = vec![
        tool_call(
            "call-shell",
            "run_shell",
            serde_json::json!({"command": "bash -lc \"printf lines\""}),
        ),
        tool_result("call-shell", "run_shell", &output),
    ];
    let theme = Theme::default();
    let contents = rendered_screen(&events, &theme, 80, 16);

    assert!(contents.contains("bash $ printf lines"));
    assert!(contents.contains("  line 1"));
    assert!(contents.contains("  line 2"));
    assert!(contents.contains("8 more lines"));
    assert!(contents.contains("ctrl+o expand"));
    assert!(contents.contains("  line 11"));
    assert!(contents.contains("  line 12"));
    assert!(!contents.contains("line 3"));
}

#[test]
fn tui_tool_output_trims_trailing_blank_rows_before_rendering() {
    let theme = Theme::default();
    let items = vec![TranscriptItem::ToolRun {
        command: "printf blank".to_owned(),
        ok: true,
        error: String::new(),
        output: "visible\n\n\n\n\n".to_owned(),
        exit_code: None,
    }];

    let texts = line_texts(&render_items_for_history(&items, &theme, 80));

    // title + body + hairline under the meaningful tool block
    assert_eq!(texts.len(), 3, "texts: {texts:?}");
    assert!(texts[0].contains("bash $ printf blank"), "texts: {texts:?}");
    assert!(texts[0].contains("done · 1 line"), "texts: {texts:?}");
    assert!(display_width(&texts[0]) <= 80, "texts: {texts:?}");
    assert!(display_width(&texts[1]) <= 80, "texts: {texts:?}");
    assert_no_box_chars(&texts);
    assert!(texts[1].contains("visible"), "texts: {texts:?}");
    assert_eq!(
        texts
            .iter()
            .filter(|line| line.contains("visible") && !line.contains('─'))
            .count(),
        1,
        "trailing blank rows should be trimmed before rendering: {texts:?}"
    );
}

#[test]
fn tool_artifact_cell_handles_empty_output_without_fold_affordance() {
    let theme = Theme::default();
    let items = vec![TranscriptItem::ToolRun {
        command: "true".to_owned(),
        ok: true,
        error: String::new(),
        output: String::new(),
        exit_code: None,
    }];

    let texts = line_texts(&render_items_for_history(&items, &theme, 80));
    let joined = texts.join("\n");

    assert!(joined.contains("bash $ true"), "texts: {texts:?}");
    assert!(joined.contains("done · 0 lines"), "texts: {texts:?}");
    assert!(!joined.contains("ctrl+o"), "texts: {texts:?}");
    assert!(texts.len() >= 2, "empty output keeps body row: {texts:?}");
    assert!(
        texts
            .iter()
            .any(|line| line.trim().is_empty() || line.contains("  ")),
        "texts: {texts:?}"
    );
    assert_no_box_chars(&texts);
}

#[test]
fn tool_artifact_cell_folds_only_above_threshold() {
    let theme = Theme::default();
    let exact_output = (1..=DEFAULT_OUTPUT_LIMIT_LINES)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let overflowing_output = (1..=DEFAULT_OUTPUT_LIMIT_LINES + 1)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");

    let exact = line_texts(&render_items_for_history(
        &[TranscriptItem::ToolRun {
            command: "printf exact".to_owned(),
            ok: true,
            error: String::new(),
            output: exact_output,
            exit_code: Some(0),
        }],
        &theme,
        80,
    ))
    .join("\n");
    let overflow = line_texts(&render_items_for_history(
        &[TranscriptItem::ToolRun {
            command: "printf overflow".to_owned(),
            ok: true,
            error: String::new(),
            output: overflowing_output,
            exit_code: Some(0),
        }],
        &theme,
        80,
    ))
    .join("\n");

    assert!(!exact.contains("more lines"), "exact: {exact:?}");
    assert!(exact.contains("line 10"), "exact: {exact:?}");
    assert!(overflow.contains("7 more lines"), "overflow: {overflow:?}");
    assert!(overflow.contains("ctrl+o expand"), "overflow: {overflow:?}");
    assert!(!overflow.contains("line 3"), "overflow: {overflow:?}");
}

#[test]
fn tool_artifact_cell_expands_with_unbounded_limit() {
    let theme = Theme::default();
    let output = (1..=12)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let item = [TranscriptItem::ToolRun {
        command: "printf lines".to_owned(),
        ok: true,
        error: String::new(),
        output,
        exit_code: Some(0),
    }];

    let folded = line_texts(&render_items_for_history(&item, &theme, 80)).join("\n");
    let expanded = line_texts(&render_items_for_history_with_limit(
        &item,
        &theme,
        80,
        usize::MAX,
    ))
    .join("\n");

    assert!(folded.contains("8 more lines"), "folded: {folded:?}");
    assert!(!folded.contains("line 3"), "folded: {folded:?}");
    assert!(!expanded.contains("more lines"), "expanded: {expanded:?}");
    assert!(expanded.contains("line 3"), "expanded: {expanded:?}");
    assert!(
        expanded.contains("exit 0 · 12 lines"),
        "expanded: {expanded:?}"
    );
}

#[test]
fn tool_artifact_flat_style_survives_fold_and_expand() {
    let theme = Theme::default();
    let output = (1..=12)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let item = [TranscriptItem::ToolRun {
        command: "find . -maxdepth 2".to_owned(),
        ok: true,
        error: String::new(),
        output,
        exit_code: Some(0),
    }];

    let folded_lines = render_items_for_history(&item, &theme, 80);
    let folded_text = line_texts(&folded_lines).join("\n");
    assert!(
        folded_text.contains("more lines"),
        "folded: {folded_text:?}"
    );
    assert_artifact_flat_style(
        "folded",
        &folded_lines,
        "find . -maxdepth 2",
        artifact_border_expected_style(&theme),
    );

    let expanded_lines = render_items_for_history_with_limit(&item, &theme, 80, usize::MAX);
    let expanded_text = line_texts(&expanded_lines).join("\n");
    assert!(
        !expanded_text.contains("more lines"),
        "expanded: {expanded_text:?}"
    );
    assert_artifact_flat_style(
        "expanded",
        &expanded_lines,
        "find . -maxdepth 2",
        artifact_border_expected_style(&theme),
    );
    assert!(
        expanded_lines.len() > folded_lines.len(),
        "folded: {folded_text:?}; expanded: {expanded_text:?}"
    );
}

#[test]
fn tool_artifact_flat_style_handles_empty_output() {
    let theme = Theme::default();
    let item = [TranscriptItem::ToolRun {
        command: "printf empty".to_owned(),
        ok: true,
        error: String::new(),
        output: String::new(),
        exit_code: Some(0),
    }];

    for (label, lines) in [
        ("normal", render_items_for_history(&item, &theme, 80)),
        (
            "unbounded",
            render_items_for_history_with_limit(&item, &theme, 80, usize::MAX),
        ),
    ] {
        assert_artifact_flat_style(
            label,
            &lines,
            "printf empty",
            artifact_border_expected_style(&theme),
        );
    }
}

#[test]
fn tool_artifact_cell_sanitizes_controls_tabs_and_bounds_width() {
    let theme = Theme::default();
    let item = [TranscriptItem::ToolRun {
        command: "printf color".to_owned(),
        ok: true,
        error: String::new(),
        output: "\u{1b}[31mred\u{1b}[0m\twide 🧔‍♂\u{8}tail\u{202e}\u{200b}\u{2060}\u{feff}"
            .to_owned(),
        exit_code: Some(0),
    }];

    for width in [8, 12, 24, 80] {
        let texts = line_texts(&render_items_for_history(&item, &theme, width));
        let joined = texts.join("\n");
        // Fixed 9-cell ledger gutter floors the minimum row width; when the
        // terminal is narrower than that floor, rows stay at the floor.
        let budget = usize::from(width).max(crate::ui::text::GUTTER_WIDTH + 4);

        assert!(!joined.contains('\u{1b}'), "width {width}: {joined:?}");
        assert!(!joined.contains('\u{8}'), "width {width}: {joined:?}");
        assert!(!joined.contains('\u{202e}'), "width {width}: {joined:?}");
        assert!(!joined.contains('\u{200b}'), "width {width}: {joined:?}");
        assert!(!joined.contains('\u{2060}'), "width {width}: {joined:?}");
        assert!(!joined.contains('\u{feff}'), "width {width}: {joined:?}");
        // At sub-gutter+body widths the body truncates "red" to a prefix.
        assert!(
            joined.contains("red") || joined.contains("re") || width <= 8,
            "width {width}: {joined:?}"
        );
        for text in &texts {
            assert!(
                display_width(text) <= budget,
                "line exceeds artifact budget at width {width}: {text:?} in {texts:?}"
            );
        }
    }
}

#[test]
fn tool_artifact_cell_uses_available_width_for_long_command_title() {
    let theme = Theme::default();
    let command = "ls -la && sed -n '1,220p' Cargo.toml && cat crates/euler-cli/Cargo.toml";
    let item = [TranscriptItem::ToolRun {
        command: command.to_owned(),
        ok: true,
        error: String::new(),
        output: "done".to_owned(),
        exit_code: Some(0),
    }];

    let wide = line_texts(&render_items_for_history(&item, &theme, 120));
    assert!(display_width(&wide[0]) <= 120, "wide: {wide:?}");
    assert!(
        wide[0].contains("crates/euler-cli/Cargo.toml"),
        "wide title should use terminal width before truncating: {wide:?}"
    );

    let narrow = line_texts(&render_items_for_history(&item, &theme, 40));
    assert!(
        display_width(&narrow[0]) <= 40,
        "narrow title should stay width-bounded: {narrow:?}"
    );
    for text in &narrow {
        assert!(
            display_width(text) <= 40,
            "narrow line exceeds terminal width: {text:?} in {narrow:?}"
        );
    }
}

#[test]
fn tool_artifact_cell_keeps_minimum_width_at_tiny_widths() {
    let theme = Theme::default();
    let item = [TranscriptItem::ToolRun {
        command: "tiny terminal".to_owned(),
        ok: true,
        error: String::new(),
        output: "ok".to_owned(),
        exit_code: Some(0),
    }];

    for width in [0, 1, 2, 3] {
        let texts = line_texts(&render_items_for_history(&item, &theme, width));
        // title + body + hairline; gutter is fixed 9 cells, body floor is 4.
        assert_eq!(texts.len(), 3, "width {width}: {texts:?}");
        for text in &texts {
            assert!(
                display_width(text) >= crate::ui::text::GUTTER_WIDTH,
                "tiny-width artifact rows keep the ledger gutter floor: {text:?} in {texts:?}"
            );
            assert!(
                display_width(text) <= crate::ui::text::GUTTER_WIDTH + 4,
                "tiny-width artifact rows stay near the minimum flat width: {text:?} in {texts:?}"
            );
        }
    }
}

#[test]
fn tool_artifact_cell_reports_failure_without_exit_code() {
    let theme = Theme::default();
    let item = [TranscriptItem::ToolRun {
        command: "cat missing".to_owned(),
        ok: false,
        error: "permission denied".to_owned(),
        output: String::new(),
        exit_code: None,
    }];

    let texts = line_texts(&render_items_for_history(&item, &theme, 80)).join("\n");

    assert!(texts.contains("✗ permission denied"), "texts: {texts:?}");
    assert!(texts.contains("0 lines"), "texts: {texts:?}");
    assert!(
        !texts.contains("failed: permission denied"),
        "texts: {texts:?}"
    );
}

#[test]
fn patch_artifact_cells_are_bounded_and_keep_independent_borders() {
    let theme = Theme::default();
    let long_path = "crates/euler-cli/src/ui/transcript/very/deep/path/with spaces/cells.rs";
    let item = [TranscriptItem::PatchProposed {
        path: long_path.to_owned(),
        old: Some("fn old() {\n\tlet value = \"\u{1b}[31mred\";\n}\n".to_owned()),
        new: Some("fn old() {\n\tlet value = \"界界界界界界界界界界界界\";\n}\n".to_owned()),
    }];

    for width in [12, 24, 64, 96] {
        let texts = line_texts(&render_items_for_history(&item, &theme, width));
        let budget = usize::from(width).max(crate::ui::text::GUTTER_WIDTH + 4);
        assert!(
            texts
                .first()
                .is_some_and(|line| line.trim_start().starts_with("Pat")),
            "width {width}: {texts:?}"
        );
        for text in &texts {
            assert!(
                display_width(text) <= budget,
                "line exceeds artifact budget at width {width}: {text:?} in {texts:?}"
            );
            assert!(!text.contains('\u{1b}'), "escape leaked: {texts:?}");
            assert!(!text.contains('\t'), "tab leaked: {texts:?}");
            assert!(!text.contains('\r'), "carriage return leaked: {texts:?}");
            assert_no_box_chars(std::slice::from_ref(text));
        }
    }
}

#[test]
fn patch_proposed_artifact_uses_boxed_title_and_not_old_child_rows() {
    let theme = Theme::default();
    let items = [
        TranscriptItem::PatchProposed {
            path: "src/a.rs".to_owned(),
            old: Some("a\n".to_owned()),
            new: Some("aa\n".to_owned()),
        },
        TranscriptItem::PatchProposed {
            path: "src/b.rs".to_owned(),
            old: Some("b\n".to_owned()),
            new: Some("bb\n".to_owned()),
        },
    ];

    let texts = line_texts(&render_items_for_history(&items, &theme, 80));
    let joined = texts.join("\n");

    assert_eq!(
        joined.matches("Patch proposed").count(),
        2,
        "texts: {texts:?}"
    );
    assert!(joined.contains("Patch proposed src/a.rs"));
    assert!(joined.contains("     1 - a"));
    assert!(
        joined.contains("@@ -1 +1 @@"),
        "hunk header missing: {texts:?}"
    );
    assert_no_box_chars(&texts);
    assert!(
        !joined.contains("* Patch proposed") && !joined.contains("• Patch proposed"),
        "old parent row leaked: {texts:?}"
    );
    assert!(
        !joined.contains("  └ @@"),
        "old child row leaked: {texts:?}"
    );
}

#[test]
fn patch_artifact_preserves_diff_styles_inside_cell_body() {
    let theme = Theme::default();
    let item = [TranscriptItem::PatchApplied {
        path: "src/main.rs".to_owned(),
        old: Some("fn old_name() {}\n".to_owned()),
        new: Some("pub fn new_name() {}\n".to_owned()),
    }];

    let lines = render_items_for_history(&item, &theme, 80);
    let inserted = lines
        .iter()
        .find(|line| line_text(line).contains("1 + pub fn new_name"))
        .expect("inserted diff row");

    assert!(
        inserted
            .spans
            .iter()
            .any(|span| span.style == theme.scopes.diff.inserted),
        "inserted row lost diff style: {inserted:?}"
    );
}

#[test]
fn patch_artifact_is_not_controlled_by_shell_fold_limit() {
    let theme = Theme::default();
    let old = (1..=20)
        .map(|index| format!("old {index}\n"))
        .collect::<String>();
    let new = "new\n".to_owned();
    let item = [TranscriptItem::PatchApplied {
        path: "src/lib.rs".to_owned(),
        old: Some(old),
        new: Some(new),
    }];

    let bounded = line_texts(&render_items_for_history(&item, &theme, 80));
    let shell_expanded = line_texts(&render_items_for_history_with_limit(
        &item,
        &theme,
        80,
        usize::MAX,
    ));

    assert_eq!(bounded, shell_expanded);
    let joined = bounded.join("\n");
    assert!(!joined.contains("bounded patch"), "bounded: {bounded:?}");
    assert!(joined.contains("ctrl+o expand"), "bounded: {bounded:?}");
    assert!(joined.contains("update · "), "bounded: {bounded:?}");
    assert!(joined.contains("visible rows"), "bounded: {bounded:?}");
}

#[test]
fn patch_proposed_artifact_is_not_controlled_by_shell_fold_limit() {
    let theme = Theme::default();
    let old = (1..=20)
        .map(|index| format!("old {index}\n"))
        .collect::<String>();
    let new = "new\n".to_owned();
    let item = [TranscriptItem::PatchProposed {
        path: "src/lib.rs".to_owned(),
        old: Some(old),
        new: Some(new),
    }];

    let bounded = line_texts(&render_items_for_history(&item, &theme, 80));
    let shell_expanded = line_texts(&render_items_for_history_with_limit(
        &item,
        &theme,
        80,
        usize::MAX,
    ));

    assert_eq!(bounded, shell_expanded);
    let joined = bounded.join("\n");
    assert!(!joined.contains("bounded patch"), "bounded: {bounded:?}");
    assert!(joined.contains("ctrl+o expand"), "bounded: {bounded:?}");
    assert!(joined.contains("update · "), "bounded: {bounded:?}");
    assert!(joined.contains("visible rows"), "bounded: {bounded:?}");
}

#[test]
fn path_only_patch_artifact_uses_fallback_title_and_body() {
    let theme = Theme::default();
    let item = [TranscriptItem::PatchApplied {
        path: "src/lib.rs".to_owned(),
        old: None,
        new: None,
    }];

    let texts = line_texts(&render_items_for_history(&item, &theme, 80));
    let joined = texts.join("\n");

    assert!(joined.contains("edit src/lib.rs"));
    assert!(joined.contains("no line changes"));
    assert!(joined.contains("unknown · 1 visible rows"));
    assert!(!joined.contains("* Edited"));
}

#[test]
fn patch_applied_artifact_keeps_exact_render_shape() {
    let theme = Theme::default();
    let item = [TranscriptItem::PatchApplied {
        path: "src/lib.rs".to_owned(),
        old: Some("a\n".to_owned()),
        new: Some("b\n".to_owned()),
    }];

    let texts = line_texts(&render_items_for_history(&item, &theme, 48));

    assert_eq!(
        texts,
        vec![
            "         edit src/lib.rs · +1 −1 · update · 3 vi",
            "                  @@ -1 +1 @@                   ",
            "              1 - a                             ",
            "              1 + b                             ",
            "         ───────────────────────────────────────",
        ]
    );
}

#[test]
fn projects_file_change_metadata_events() {
    let events = vec![event(
        EventKind::FILE_CHANGE,
        object([
            ("path", "src/lib.rs".into()),
            ("action", "modify".into()),
            ("origin", "apply_patch".into()),
            ("before_sha256", "abcdef1234567890".into()),
            ("after_sha256", "fedcba6543210000".into()),
            ("before_byte_len", 10_u64.into()),
            ("after_byte_len", 12_u64.into()),
            ("diff_redaction", "omitted".into()),
        ]),
    )];

    assert_eq!(
        project_events(&events),
        vec![TranscriptItem::FileChange {
            path: "src/lib.rs".to_owned(),
            action: "modify".to_owned(),
            origin: "apply_patch".to_owned(),
            before_sha256: Some("abcdef1234567890".to_owned()),
            after_sha256: Some("fedcba6543210000".to_owned()),
            before_byte_len: Some(10),
            after_byte_len: Some(12),
            diff_redaction: "omitted".to_owned(),
            checkpoint_event_id: None,
        }]
    );
}

#[test]
fn projects_file_diff_artifact_events() {
    let events = vec![event(
        EventKind::FILE_DIFF,
        object([
            ("path", "src/lib.rs".into()),
            ("action", "modify".into()),
            ("origin", "apply_patch".into()),
            ("diff", "--- a/src/lib.rs\n+++ b/src/lib.rs\n+new\n".into()),
            ("truncated", true.into()),
            ("truncation", "tail".into()),
            ("omitted_reason", "diff exceeded 65536 bytes".into()),
        ]),
    )];

    assert_eq!(
        project_events(&events),
        vec![TranscriptItem::FileDiff {
            path: "src/lib.rs".to_owned(),
            action: "modify".to_owned(),
            origin: "apply_patch".to_owned(),
            diff: Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n+new\n".to_owned()),
            truncated: true,
            truncation: "tail".to_owned(),
            omitted_reason: Some("diff exceeded 65536 bytes".to_owned()),
            checkpoint_event_id: None,
        }]
    );
}

#[test]
fn projects_checkpoint_suffix_and_workspace_restore() {
    let change = event(
        EventKind::FILE_CHANGE,
        object([
            ("path", "src/lib.rs".into()),
            ("action", "modify".into()),
            ("origin", "edit_file".into()),
            ("pre_image_blob", "abc".into()),
        ]),
    );
    let change_id = change.id.clone();
    let events = vec![
        change,
        event(
            EventKind::FILE_DIFF,
            object([
                ("path", "src/lib.rs".into()),
                ("action", "modify".into()),
                ("origin", "edit_file".into()),
                ("file_change_id", change_id.clone().into()),
                ("diff", "--- a/src/lib.rs\n+++ b/src/lib.rs\n+new\n".into()),
                ("truncated", false.into()),
                ("truncation", "none".into()),
                ("omitted_reason", serde_json::Value::Null),
            ]),
        ),
        event(
            EventKind::WORKSPACE_RESTORE,
            object([
                ("path", "src/lib.rs".into()),
                ("checkpoint_event_id", change_id.clone().into()),
                ("blob_sha256", "abc".into()),
                ("restored", true.into()),
            ]),
        ),
    ];

    let projected = project_events(&events);
    assert_eq!(
        projected[0],
        TranscriptItem::FileChange {
            path: "src/lib.rs".to_owned(),
            action: "modify".to_owned(),
            origin: "edit_file".to_owned(),
            before_sha256: None,
            after_sha256: None,
            before_byte_len: None,
            after_byte_len: None,
            diff_redaction: String::new(),
            checkpoint_event_id: Some(change_id.clone()),
        }
    );
    assert_eq!(
        projected[1],
        TranscriptItem::FileDiff {
            path: "src/lib.rs".to_owned(),
            action: "modify".to_owned(),
            origin: "edit_file".to_owned(),
            diff: Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n+new\n".to_owned()),
            truncated: false,
            truncation: "none".to_owned(),
            omitted_reason: None,
            checkpoint_event_id: Some(change_id.clone()),
        }
    );
    assert_eq!(
        projected[2],
        TranscriptItem::WorkspaceRestore {
            path: "src/lib.rs".to_owned(),
            checkpoint_event_id: change_id.clone(),
        }
    );

    let theme = Theme::default();
    let joined = line_texts(&render_items_for_history(&projected[2..], &theme, 120)).join("\n");
    assert!(
        joined.contains(&format!("↩ reverted src/lib.rs → ckpt {change_id}")),
        "joined: {joined:?}"
    );
    assert!(
        joined.contains("files restored, history intact"),
        "joined: {joined:?}"
    );
}

#[test]
fn line_oriented_renderer_names_file_change_action_and_path() {
    let events = vec![event(
        EventKind::FILE_CHANGE,
        object([("path", "src/lib.rs".into()), ("action", "modify".into())]),
    )];

    assert_eq!(
        render_line_oriented(&events),
        "file.change: modify: src/lib.rs\n"
    );
}

#[test]
fn line_oriented_renderer_names_file_diff_action_path_and_omission() {
    let events = vec![
        event(
            EventKind::FILE_DIFF,
            object([
                ("path", "src/lib.rs".into()),
                ("action", "modify".into()),
                ("diff", "@@ -1 +1 @@\n-a\n+b\n".into()),
            ]),
        ),
        event(
            EventKind::FILE_DIFF,
            object([
                ("path", "secret.txt".into()),
                ("action", "modify".into()),
                ("diff", serde_json::Value::Null),
            ]),
        ),
    ];

    assert_eq!(
        render_line_oriented(&events),
        "file.diff: modify: src/lib.rs\nfile.diff: modify: secret.txt (omitted)\n"
    );
}

#[test]
fn file_change_metadata_renders_as_flat_artifact_without_fake_diff() {
    let theme = Theme::default();
    let item = [TranscriptItem::FileChange {
        path: "src/lib.rs".to_owned(),
        action: "modify".to_owned(),
        origin: "apply_patch".to_owned(),
        before_sha256: Some("abcdef1234567890".to_owned()),
        after_sha256: Some("fedcba6543210000".to_owned()),
        before_byte_len: Some(10),
        after_byte_len: Some(12),
        diff_redaction: "omitted".to_owned(),
        checkpoint_event_id: None,
    }];

    let texts = line_texts(&render_items_for_history(&item, &theme, 80));
    let joined = texts.join("\n");

    assert!(joined.contains("File modified src/lib.rs · metadata only"));
    assert!(joined.contains("  action: modify"));
    assert!(joined.contains("  origin: apply_patch"));
    assert!(joined.contains("  bytes: 10 -> 12"));
    assert!(joined.contains("  sha256: abcdef123456 -> fedcba654321"));
    assert!(joined.contains("  diff: omitted (metadata only)"));
    assert_no_box_chars(&texts);
    assert!(!joined.contains("@@"));
    assert!(!joined.contains("+         1 |"));
    assert!(!joined.contains("-         1 |"));
}

#[test]
fn file_diff_renders_unified_diff_as_source_first_artifact() {
    let theme = Theme::default();
    let item = [TranscriptItem::FileDiff {
        path: "src/lib.rs".to_owned(),
        action: "modify".to_owned(),
        origin: "apply_patch".to_owned(),
        diff: Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n".to_owned()),
        truncated: false,
        truncation: "none".to_owned(),
        omitted_reason: None,
        checkpoint_event_id: None,
    }];

    let texts = line_texts(&render_items_for_history(&item, &theme, 80));
    let joined = texts.join("\n");

    assert!(joined.contains("edit src/lib.rs · +1 −1"));
    assert!(joined.contains("     1 - old"));
    assert!(joined.contains("     1 + new"));
    assert_no_box_chars(&texts);
    assert!(!joined.contains("--- a/src/lib.rs"));
    assert!(!joined.contains("+++ b/src/lib.rs"));
    assert!(joined.contains("@@ -1 +1 @@"));
}

#[test]
fn file_diff_omission_renders_reason_without_metadata_inference() {
    let theme = Theme::default();
    let item = [TranscriptItem::FileDiff {
        path: "src/lib.rs".to_owned(),
        action: "modify".to_owned(),
        origin: "edit_file".to_owned(),
        diff: None,
        truncated: false,
        truncation: "none".to_owned(),
        omitted_reason: Some("secret-like".to_owned()),
        checkpoint_event_id: None,
    }];

    let joined = line_texts(&render_items_for_history(&item, &theme, 80)).join("\n");

    assert!(joined.contains("edit src/lib.rs"));
    assert!(joined.contains("  diff: omitted: secret-like"));
    assert!(!joined.contains("@@"));
    assert!(!joined.contains("File modified"));
}

#[test]
fn file_diff_missing_omission_reason_uses_stable_fallback() {
    let theme = Theme::default();
    let item = [TranscriptItem::FileDiff {
        path: "src/lib.rs".to_owned(),
        action: "modify".to_owned(),
        origin: "edit_file".to_owned(),
        diff: None,
        truncated: false,
        truncation: "none".to_owned(),
        omitted_reason: None,
        checkpoint_event_id: None,
    }];

    let joined = line_texts(&render_items_for_history(&item, &theme, 80)).join("\n");

    assert!(joined.contains("  diff: diff omitted"));
}

#[test]
fn file_diff_empty_diff_is_present_not_omitted() {
    let theme = Theme::default();
    let item = [TranscriptItem::FileDiff {
        path: "src/lib.rs".to_owned(),
        action: "modify".to_owned(),
        origin: "edit_file".to_owned(),
        diff: Some(String::new()),
        truncated: false,
        truncation: "none".to_owned(),
        omitted_reason: Some("should not render".to_owned()),
        checkpoint_event_id: None,
    }];

    let joined = line_texts(&render_items_for_history(&item, &theme, 80)).join("\n");

    assert!(joined.contains("    no diff lines"));
    assert!(!joined.contains("omitted"));
    assert!(!joined.contains("should not render"));
}

#[test]
fn file_diff_whitespace_only_diff_is_present_not_omitted() {
    let theme = Theme::default();
    let item = [TranscriptItem::FileDiff {
        path: "src/lib.rs".to_owned(),
        action: "modify".to_owned(),
        origin: "edit_file".to_owned(),
        diff: Some("\n\n".to_owned()),
        truncated: false,
        truncation: "none".to_owned(),
        omitted_reason: Some("should not render".to_owned()),
        checkpoint_event_id: None,
    }];

    let joined = line_texts(&render_items_for_history(&item, &theme, 80)).join("\n");

    assert!(joined.contains("    no diff lines"));
    assert!(!joined.contains("omitted"));
    assert!(!joined.contains("should not render"));
}

#[test]
fn file_diff_ignores_shell_artifact_limit_and_renders_full_code() {
    let theme = Theme::default();
    let diff = (1..=12)
        .map(|index| format!("+line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let item = [TranscriptItem::FileDiff {
        path: "src/lib.rs".to_owned(),
        action: "modify".to_owned(),
        origin: "apply_patch".to_owned(),
        diff: Some(diff),
        truncated: true,
        truncation: "tail".to_owned(),
        omitted_reason: Some("diff exceeded 65536 bytes".to_owned()),
        checkpoint_event_id: None,
    }];

    let default = line_texts(&render_items_for_history(&item, &theme, 80)).join("\n");
    let expanded = line_texts(&render_items_for_history_with_limit(
        &item,
        &theme,
        80,
        usize::MAX,
    ))
    .join("\n");

    assert_ne!(default, expanded);
    assert!(
        !default.contains("hidden diff lines"),
        "default: {default:?}"
    );
    assert!(default.contains("ctrl+o expand"), "default: {default:?}");
    assert!(!default.contains("line 11"), "default: {default:?}");
    assert!(expanded.contains("line 11"), "expanded: {expanded:?}");
    // Title row is `edit path · +N −M · action · lines · origin · truncated …`;
    // at width 80 the 9-cell gutter can soft-truncate the final "tail".
    assert!(
        default.contains("modify · 12 lines · apply_patch · truncated"),
        "default: {default:?}"
    );
}

#[test]
fn file_diff_sanitizes_controls_and_bounds_width() {
    let theme = Theme::default();
    let item = [TranscriptItem::FileDiff {
        path: "src/\u{1b}[31mvery-long-path-with-control\nfile.rs".to_owned(),
        action: "modify".to_owned(),
        origin: "apply\tpatch\u{1b}]52;c;bad\u{7}".to_owned(),
        diff: Some(format!(
            "--- a/src/lib.rs\r\n+++ b/src/lib.rs\n+\t{}\u{1b}[31mred\u{1b}[0m\rBAD\u{8}\u{202e}",
            "x".repeat(50_000)
        )),
        truncated: false,
        truncation: "none".to_owned(),
        omitted_reason: None,
        checkpoint_event_id: None,
    }];

    let texts = line_texts(&render_items_for_history(&item, &theme, 36));
    let joined = texts.join("\n");

    assert!(!joined.contains('\u{1b}'));
    assert!(!joined.contains('\u{7}'));
    assert!(!joined.contains('\u{202e}'));
    assert!(!joined.contains("[31m"));
    assert!(!joined.contains("BAD"));
    for row in texts {
        assert!(
            display_width(&row) <= 36,
            "row overflowed narrow artifact width: {row:?}"
        );
    }
}

#[test]
fn file_change_and_file_diff_artifacts_are_distinct_and_ordered() {
    let events = vec![
        event(
            EventKind::FILE_CHANGE,
            object([
                ("path", "src/lib.rs".into()),
                ("action", "modify".into()),
                ("origin", "apply_patch".into()),
                ("before_byte_len", 2_u64.into()),
                ("after_byte_len", 2_u64.into()),
                ("diff_redaction", "omitted".into()),
            ]),
        ),
        event(
            EventKind::FILE_DIFF,
            object([
                ("path", "src/lib.rs".into()),
                ("action", "modify".into()),
                ("origin", "apply_patch".into()),
                ("diff", "@@ -1 +1 @@\n-a\n+b\n".into()),
            ]),
        ),
    ];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 80, 18);
    let change_index = contents
        .find("File modified src/lib.rs · metadata only")
        .expect("file change cell");
    let diff_index = contents
        .find("edit src/lib.rs · +1 −1")
        .expect("file diff cell");

    assert!(change_index < diff_index, "contents: {contents:?}");
    assert!(contents.contains("  diff: omitted (metadata only)"));
    assert!(contents.contains("     1 - a"));
    assert!(contents.contains("     1 + b"));
    assert!(contents.contains("@@ -1 +1 @@"));

    let change_block = &contents[change_index..diff_index];
    assert!(
        !change_block.contains("@@"),
        "change block: {change_block:?}"
    );
    assert!(
        !change_block.contains(" 1 - a") && !change_block.contains(" 1 + b"),
        "change block: {change_block:?}"
    );
}

#[test]
fn patch_file_change_and_file_diff_render_independently_in_event_order() {
    let events = vec![
        event(
            EventKind::PATCH_APPLIED,
            object([
                ("path", "src/lib.rs".into()),
                ("old", "a\n".into()),
                ("new", "b\n".into()),
            ]),
        ),
        event(
            EventKind::FILE_CHANGE,
            object([
                ("path", "src/lib.rs".into()),
                ("action", "modify".into()),
                ("origin", "apply_patch".into()),
                ("diff_redaction", "omitted".into()),
            ]),
        ),
        event(
            EventKind::FILE_DIFF,
            object([
                ("path", "src/lib.rs".into()),
                ("action", "modify".into()),
                ("origin", "apply_patch".into()),
                ("diff", "@@ -1 +1 @@\n-a\n+b\n".into()),
            ]),
        ),
    ];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 96, 24);
    let patch_index = contents.find("edit src/lib.rs").expect("patch cell");
    let change_index = contents
        .find("File modified src/lib.rs · metadata only")
        .expect("file change cell");
    let diff_index = contents
        .rfind("edit src/lib.rs · +1 −1")
        .expect("file diff cell");

    assert!(patch_index < change_index, "contents: {contents:?}");
    assert!(change_index < diff_index, "contents: {contents:?}");
    assert!(contents.contains("     1 - a"));
    assert!(!contents.contains("  -a"));
}

#[test]
fn file_change_add_action_and_one_sided_hashes_render_stable_metadata() {
    let theme = Theme::default();
    let item = [TranscriptItem::FileChange {
        path: "src/new.rs".to_owned(),
        action: "add".to_owned(),
        origin: "edit_file".to_owned(),
        before_sha256: None,
        after_sha256: Some("0123456789abcdef".to_owned()),
        before_byte_len: None,
        after_byte_len: Some(42),
        diff_redaction: "omitted".to_owned(),
        checkpoint_event_id: None,
    }];

    let joined = line_texts(&render_items_for_history(&item, &theme, 80)).join("\n");

    assert!(joined.contains("File added src/new.rs · metadata only"));
    assert!(joined.contains("  action: add"));
    assert!(joined.contains("  bytes: unknown -> 42"));
    assert!(joined.contains("  sha256: none -> 0123456789ab"));
}

#[test]
fn file_change_action_is_normalized_for_title_and_body() {
    let theme = Theme::default();
    let item = [TranscriptItem::FileChange {
        path: "src/lib.rs".to_owned(),
        action: "\u{1b}[31mmodify\u{1b}[0m\n".to_owned(),
        origin: String::new(),
        before_sha256: None,
        after_sha256: None,
        before_byte_len: None,
        after_byte_len: None,
        diff_redaction: "custom-redaction".to_owned(),
        checkpoint_event_id: None,
    }];

    let joined = line_texts(&render_items_for_history(&item, &theme, 80)).join("\n");

    assert!(joined.contains("File modified src/lib.rs · metadata only"));
    assert!(joined.contains("  action: modify"));
    assert!(joined.contains("  diff: custom-redaction"));
    assert!(!joined.contains('\u{1b}'));
}

#[test]
fn multiple_file_change_events_render_separately_in_order() {
    let theme = Theme::default();
    let items = [
        TranscriptItem::FileChange {
            path: "src/a.rs".to_owned(),
            action: "add".to_owned(),
            origin: "edit_file".to_owned(),
            before_sha256: None,
            after_sha256: None,
            before_byte_len: None,
            after_byte_len: Some(1),
            diff_redaction: "omitted".to_owned(),
            checkpoint_event_id: None,
        },
        TranscriptItem::FileChange {
            path: "src/b.rs".to_owned(),
            action: "modify".to_owned(),
            origin: "apply_patch".to_owned(),
            before_sha256: None,
            after_sha256: None,
            before_byte_len: Some(1),
            after_byte_len: Some(2),
            diff_redaction: "omitted".to_owned(),
            checkpoint_event_id: None,
        },
    ];

    let joined = line_texts(&render_items_for_history(&items, &theme, 80)).join("\n");
    let first = joined
        .find("File added src/a.rs · metadata only")
        .expect("first cell");
    let second = joined
        .find("File modified src/b.rs · metadata only")
        .expect("second cell");

    assert!(first < second, "joined: {joined:?}");
}

#[test]
fn file_change_sparse_metadata_uses_stable_fallbacks() {
    let theme = Theme::default();
    let item = [TranscriptItem::FileChange {
        path: String::new(),
        action: String::new(),
        origin: String::new(),
        before_sha256: None,
        after_sha256: None,
        before_byte_len: None,
        after_byte_len: None,
        diff_redaction: String::new(),
        checkpoint_event_id: None,
    }];

    let joined = line_texts(&render_items_for_history(&item, &theme, 80)).join("\n");

    assert!(joined.contains("File changed (unknown path) · metadata only"));
    assert!(joined.contains("  action: unknown"));
    assert!(!joined.contains("  origin:"));
    assert!(joined.contains("  bytes: unknown -> unknown"));
    assert!(!joined.contains("  sha256:"));
    assert!(joined.contains("  diff: metadata only"));
}

#[test]
fn file_change_metadata_is_sanitized_and_width_bounded() {
    let theme = Theme::default();
    let item = [TranscriptItem::FileChange {
        path: "src/\u{1b}[31mvery-long-path-with-control\nfile.rs".to_owned(),
        action: "modify".to_owned(),
        origin: "apply\tpatch\u{1b}]52;c;bad\u{7}".to_owned(),
        before_sha256: Some("abc".to_owned()),
        after_sha256: Some("def456789012345".to_owned()),
        before_byte_len: Some(0),
        after_byte_len: Some(2048),
        diff_redaction: "omitted\nnever diff".to_owned(),
        checkpoint_event_id: None,
    }];

    // Width 48 keeps the sanitized origin fully visible after the 9-cell gutter.
    let texts = line_texts(&render_items_for_history(&item, &theme, 48));
    let joined = texts.join("\n");

    assert!(!joined.contains('\u{1b}'));
    assert!(!joined.contains('\u{7}'));
    assert!(!joined.contains("[31m"));
    assert!(
        joined.contains("origin: apply    patch"),
        "joined: {joined:?}"
    );
    assert!(joined.contains("sha256: abc"));
    assert!(joined.contains("def4567890"));
    for row in texts {
        assert!(
            display_width(&row) <= 48,
            "row overflowed narrow artifact width: {row:?}"
        );
    }
}

#[test]
fn patch_and_file_change_artifacts_are_distinct_and_ordered() {
    let events = vec![
        event(
            EventKind::PATCH_APPLIED,
            object([
                ("path", "src/lib.rs".into()),
                ("old", "a\n".into()),
                ("new", "b\n".into()),
            ]),
        ),
        event(
            EventKind::FILE_CHANGE,
            object([
                ("path", "src/lib.rs".into()),
                ("action", "modify".into()),
                ("origin", "apply_patch".into()),
                ("before_byte_len", 2_u64.into()),
                ("after_byte_len", 2_u64.into()),
                ("diff_redaction", "omitted".into()),
            ]),
        ),
    ];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 80, 18);
    let patch_index = contents.find("edit src/lib.rs").expect("patch cell");
    let change_index = contents
        .find("File modified src/lib.rs · metadata only")
        .expect("file change cell");

    assert!(patch_index < change_index, "contents: {contents:?}");
    assert!(contents.contains("     1 - a"));
    assert!(contents.contains("@@"), "hunk header missing");
    assert!(contents.contains("  diff: omitted (metadata only)"));
}

#[test]
fn consecutive_patch_artifact_cells_do_not_merge_or_use_old_child_rows() {
    let theme = Theme::default();
    let items = [
        TranscriptItem::PatchApplied {
            path: "src/a.rs".to_owned(),
            old: Some("a\n".to_owned()),
            new: Some("aa\n".to_owned()),
        },
        TranscriptItem::PatchApplied {
            path: "src/b.rs".to_owned(),
            old: Some("b\n".to_owned()),
            new: Some("bb\n".to_owned()),
        },
    ];

    let texts = line_texts(&render_items_for_history(&items, &theme, 80));
    let joined = texts.join("\n");

    assert_eq!(joined.matches("edit src/").count(), 2, "texts: {texts:?}");
    assert_no_box_chars(&texts);
    assert!(
        !joined.contains("  └ @@"),
        "old child row leaked: {texts:?}"
    );
    assert!(
        !joined.contains("• Edited"),
        "old parent row leaked: {texts:?}"
    );
}

#[test]
fn final_assistant_prose_uses_two_space_gutter_across_markdown_shapes() {
    let theme = Theme::default();
    let items = vec![TranscriptItem::AssistantMessage(
        "Paragraph text that wraps near a boundary.\n\n- listed item\n\n| A | B |\n|---|---|\n| 1 | 2 |\n"
            .to_owned(),
    )];

    let texts = line_texts(&render_items_for_history(&items, &theme, 24));

    assert!(texts.len() > 4, "texts: {texts:?}");
    for text in texts.iter().filter(|line| !line.trim().is_empty()) {
        // blank 9-cell ledger gutter, or hairline under the block
        assert!(
            text.starts_with("         ") || text.contains("─"),
            "assistant prose line missing gutter: {text:?} in {texts:?}"
        );
    }
    assert!(texts.iter().any(|line| line.contains("- listed")));
    assert!(texts.iter().any(|line| line.contains("A: 1")));
    assert!(texts.iter().any(|line| line.contains("B: 2")));
}

#[test]
fn final_assistant_prose_gutter_preserves_width_budget() {
    let theme = Theme::default();
    let items = vec![TranscriptItem::AssistantMessage(
        "Paragraph text near a boundary.\n\n- listed item with enough words to wrap\n\n| Alpha | Beta |\n|---|---|\n| one | two |\n"
            .to_owned(),
    )];

    for width in 12..=32 {
        let texts = line_texts(&render_items_for_history(&items, &theme, width));
        let budget = usize::from(width).max(crate::ui::text::GUTTER_WIDTH + 1);
        for text in texts.iter().filter(|line| !line.trim().is_empty()) {
            assert!(
                display_width(text) <= budget,
                "line exceeds width {width}: {text:?} in {texts:?}"
            );
            // With a 9-cell ledger gutter, safety-cell room only exists once
            // content_width - 1 leaves real markdown headroom (list markers, etc.).
            if width >= 16 {
                assert!(
                    display_width(text) < usize::from(width) || text.contains('─'),
                    "assistant markdown should leave a right-edge safety cell at width {width}: {text:?} in {texts:?}"
                );
            }
        }
    }
}

#[test]
fn worked_separator_degrades_to_single_bare_label_at_narrow_widths() {
    let theme = Theme::default();
    let item = [TranscriptItem::WorkedDuration("12s".to_owned())];
    let label = "Worked for 12s";

    let below = line_texts(&render_items_for_history(&item, &theme, 6));
    let equal = line_texts(&render_items_for_history(&item, &theme, label.len() as u16));
    let wide = line_texts(&render_items_for_history(&item, &theme, 32));

    assert_eq!(below, vec![label]);
    assert_eq!(equal, vec![label]);
    assert_eq!(wide.len(), 1);
    assert!(wide[0].contains(label));
    assert!(wide[0].contains('─'));
}

#[test]
fn resume_boundary_renders_decision_record_and_centered_divider() {
    let theme = Theme::default();
    let item = [TranscriptItem::ResumeBoundary {
        label: "research".to_owned(),
        recovery_closure_appended: true,
        warning_count: 1,
        events_replayed: 12,
    }];
    let texts = line_texts(&render_items_for_history(&item, &theme, 72));
    let joined = texts.join("\n");
    assert!(
        joined.contains("✓ resumed session research"),
        "joined: {joined}"
    );
    assert!(
        joined.contains("recovery closure appended"),
        "joined: {joined}"
    );
    assert!(joined.contains("warnings"), "joined: {joined}");
    assert!(
        joined.contains("12 events replayed · model context folded to stubs"),
        "joined: {joined}"
    );
    assert!(joined.contains('─'), "joined: {joined}");
}

#[test]
fn tui_long_tool_output_ignores_trailing_blanks_in_head_tail_preview() {
    let output = (1..=12)
        .map(|index| format!("line {index}"))
        .chain(std::iter::repeat_n(String::new(), 6))
        .collect::<Vec<_>>()
        .join("\n");
    let events = vec![
        tool_call(
            "call-shell",
            "run_shell",
            serde_json::json!({"command": "bash -lc \"printf lines\""}),
        ),
        tool_result("call-shell", "run_shell", &output),
    ];
    let theme = Theme::default();
    let contents = rendered_screen(&events, &theme, 80, 16);

    assert!(contents.contains("8 more lines"));
    assert!(contents.contains("ctrl+o expand"));
    assert!(contents.contains("  line 11"));
    assert!(contents.contains("  line 12"));
    assert!(!contents.contains("14 more lines"));
}

#[test]
fn vt100_overflowing_transcript_shows_latest_event() {
    let events = vec![
        event(
            EventKind::USER_MESSAGE,
            object([("content", "oldest event".into())]),
        ),
        event(
            EventKind::ASSISTANT_MESSAGE,
            object([("content", "middle event".into())]),
        ),
        event(
            EventKind::USER_MESSAGE,
            object([("content", "latest event".into())]),
        ),
    ];
    let theme = Theme::default();

    // Hairlines + turn footer consume rows; height 4 is the smallest that still
    // keeps the latest user event in the viewport under Warm Ledger rhythm.
    let contents = rendered_screen(&events, &theme, 32, 4);

    assert!(!contents.contains("oldest event"));
    assert!(contents.contains("latest event"), "contents: {contents:?}");
}

#[test]
fn vt100_opaque_reasoning_payload_never_renders() {
    let events = vec![
        event(
            EventKind::MODEL_REASONING,
            object([
                ("fidelity", "opaque".into()),
                ("content", "opaque-visible-secret".into()),
                ("artifact", "provider-secret-artifact".into()),
            ]),
        ),
        event(
            EventKind::USER_MESSAGE,
            object([("content", "visible user content".into())]),
        ),
    ];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 40, 4);

    assert!(contents.contains("visible user content"));
    assert!(!contents.contains("opaque-visible-secret"));
    assert!(!contents.contains("provider-secret-artifact"));
}

#[test]
fn vt100_reasoning_render_uses_human_header_and_clean_indent() {
    let events = vec![
        event(
            EventKind::MODEL_REASONING,
            object([
                ("fidelity", "raw".into()),
                ("content", "inspect the event projection".into()),
            ]),
        ),
        event(
            EventKind::MODEL_REASONING,
            object([
                ("fidelity", "summary".into()),
                ("content", "summarized provider reasoning".into()),
            ]),
        ),
    ];
    let theme = Theme::default();

    // Two reasoning blocks + hairlines need more than 6 rows to keep both headers.
    let contents = rendered_screen(&events, &theme, 64, 10);
    let rows = contents.lines().map(str::trim_end).collect::<Vec<_>>();

    assert!(
        rows.iter().any(|row| row.contains("✱ thought for 0s")),
        "contents: {contents:?}"
    );
    assert!(
        contents.contains("inspect the event projection"),
        "contents: {contents:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("✱ thought summary for 0s")),
        "contents: {contents:?}"
    );
    assert!(!contents.contains("* Reasoning"), "contents: {contents:?}");
    assert!(!contents.contains("|"), "contents: {contents:?}");
}

#[test]
fn vt100_failed_tool_with_exit_code_and_empty_error_has_no_dangling_colon() {
    let events = vec![event(
        EventKind::TOOL_RESULT,
        object([
            ("name", "run_shell".into()),
            ("ok", false.into()),
            ("error", "".into()),
            ("exit_code", 2.into()),
        ]),
    )];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 48, 4);

    assert!(contents.contains("bash"));
    assert!(contents.contains("✗ exit 2 · 0 lines"));
    assert!(!contents.contains("exit 2:"));
}

#[test]
fn failed_tool_run_surfaces_informative_line_before_tail() {
    let theme = Theme::default();
    let output = (1..=12)
        .map(|index| {
            if index == 5 {
                "error[E0425]: cannot find value `x`".to_owned()
            } else {
                format!("line {index}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let item = [TranscriptItem::ToolRun {
        command: "cargo test".to_owned(),
        ok: false,
        error: String::new(),
        output,
        exit_code: Some(101),
    }];

    let texts = line_texts(&render_items_for_history(&item, &theme, 80));
    let joined = texts.join("\n");

    assert!(
        joined.contains("✗ exit 101"),
        "failure verb should be loud: {joined:?}"
    );
    // Body rows are blank-gutter + two-space pad; title rows share the gutter
    // prefix so filter on the body pad after the 9-cell gutter.
    let body: Vec<_> = texts
        .iter()
        .filter(|line| line.starts_with("           ") && !line.trim().is_empty())
        .collect();
    assert!(
        body.first()
            .is_some_and(|line| line.contains("error[E0425]: cannot find value `x`")),
        "first surfaced body line should be the informative match: {texts:?}"
    );
    assert!(
        joined.contains("more lines · ctrl+o expand"),
        "fold marker should remain: {joined:?}"
    );
    assert!(
        joined.contains("line 11") && joined.contains("line 12"),
        "summary tail should remain: {joined:?}"
    );
}

#[test]
fn edit_failure_renders_path_and_cause_inline() {
    let events = vec![
        tool_call(
            "edit-1",
            "edit_file",
            serde_json::json!({
                "path": "retry.rs",
                "old": "a",
                "new": "b"
            }),
        ),
        event(
            EventKind::TOOL_RESULT,
            object([
                ("id", "edit-1".into()),
                ("name", "edit_file".into()),
                ("ok", false.into()),
                (
                    "error",
                    "hunk 2/3 did not apply — file changed on disk since read".into(),
                ),
            ]),
        ),
    ];
    let theme = Theme::default();
    let contents = rendered_screen(&events, &theme, 96, 6);

    assert!(
        contents
            .contains("edit retry.rs ✗ hunk 2/3 did not apply — file changed on disk since read"),
        "contents: {contents:?}"
    );
    assert!(!contents.contains("edit failed"), "contents: {contents:?}");
    assert!(
        !contents.contains("edit retry.rs failed"),
        "contents: {contents:?}"
    );
}

#[test]
fn interrupted_ledger_row_uses_spec_copy() {
    let theme = Theme::default();
    let texts = line_texts(&render_items_for_history(
        &[TranscriptItem::Interrupted],
        &theme,
        80,
    ));
    let joined = texts.join("\n");
    assert!(
        joined.contains("interrupted — tell euler what to do differently"),
        "texts: {texts:?}"
    );
    assert!(
        !joined.contains("Conversation interrupted"),
        "texts: {texts:?}"
    );
    assert!(!joined.contains("tell the model"), "texts: {texts:?}");
}

#[test]
fn vt100_renders_absolute_time_duration_and_turn_footer() {
    let events = vec![
        event_at(
            EventKind::USER_MESSAGE,
            object([("content", "start timing".into())]),
            "2026-06-20T14:32:07.000Z",
        ),
        event_at(
            EventKind::ASSISTANT_MESSAGE,
            object([("content", "done timing".into())]),
            "2026-06-20T14:34:00.000Z",
        ),
    ];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 80, 6);

    assert!(contents.contains("start timing · 14:32:07"));
    assert!(contents.contains("done timing · +1m 53s · 14:34:00"));
    assert!(contents.contains("─ 1m 53s · 14:34:00 ─"));
}

#[test]
fn vt100_skips_invalid_timestamps_without_breaking_transcript() {
    let events = vec![
        event_at(
            EventKind::USER_MESSAGE,
            object([("content", "bad time".into())]),
            "not-a-time",
        ),
        event_at(
            EventKind::ASSISTANT_MESSAGE,
            object([("content", "good time".into())]),
            "2026-06-20T14:32:07.000Z",
        ),
    ];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 80, 6);

    assert!(contents.contains("bad time"));
    assert!(!contents.contains("not-a-time"));
    assert!(contents.contains("good time · 14:32:07"));
}

#[test]
fn vt100_clamps_out_of_order_timestamp_duration_to_zero() {
    let events = vec![
        event_at(
            EventKind::USER_MESSAGE,
            object([("content", "later".into())]),
            "2026-06-20T14:34:00.000Z",
        ),
        event_at(
            EventKind::ASSISTANT_MESSAGE,
            object([("content", "earlier".into())]),
            "2026-06-20T14:32:07.000Z",
        ),
    ];
    let theme = Theme::default();

    let contents = rendered_screen(&events, &theme, 80, 6);

    assert!(contents.contains("earlier · +0s · 14:32:07"));
    assert!(contents.contains("─ 0s · 14:32:07 ─"));
}

#[test]
fn vt100_omits_timing_badge_when_it_would_overflow_row() {
    let events = vec![event_at(
        EventKind::USER_MESSAGE,
        object([("content", "very long row content".into())]),
        "2026-06-20T14:32:07.000Z",
    )];
    let theme = Theme::default();

    // Width 28 leaves room for the 9-cell stamp + rail + "very long row" without
    // also fitting the trailing timing badge on the same row.
    let contents = rendered_screen(&events, &theme, 28, 4);

    assert!(contents.contains("very long row"), "contents: {contents:?}");
    assert!(!contents.contains("very long row · 14:32:07"));
}

fn rendered_screen(events: &[EventEnvelope], theme: &Theme, width: u16, height: u16) -> String {
    rendered_screen_with_limit(events, theme, width, height, DEFAULT_OUTPUT_LIMIT_LINES)
}

fn rendered_screen_with_limit(
    events: &[EventEnvelope],
    theme: &Theme,
    width: u16,
    height: u16,
    limit: usize,
) -> String {
    let backend = VT100Backend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");

    terminal
        .draw(|frame| {
            frame.render_widget(
                transcript_widget(events, theme).output_limit_lines(limit),
                Rect::new(0, 0, width, height),
            );
        })
        .expect("draw");

    terminal.backend().screen_contents()
}

fn event(kind: &'static str, payload: euler_event::JsonObject) -> EventEnvelope {
    EventEnvelope::new("session", "agent", None, kind, payload)
}

fn event_at(
    kind: &'static str,
    payload: euler_event::JsonObject,
    ts: &'static str,
) -> EventEnvelope {
    let mut event = event(kind, payload);
    event.ts = ts.to_owned();
    event
}

fn tool_call(id: &str, name: &str, input: serde_json::Value) -> EventEnvelope {
    event(
        EventKind::TOOL_CALL,
        object([
            ("id", id.to_owned().into()),
            ("name", name.to_owned().into()),
            ("input", input),
        ]),
    )
}

fn tool_result(id: &str, name: &str, output: &str) -> EventEnvelope {
    event(
        EventKind::TOOL_RESULT,
        object([
            ("id", id.to_owned().into()),
            ("name", name.to_owned().into()),
            ("ok", true.into()),
            ("output", output.to_owned().into()),
        ]),
    )
}

fn line_texts(lines: &[Line<'_>]) -> Vec<String> {
    lines.iter().map(line_text).collect()
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

fn assert_artifact_flat_style(
    label: &str,
    lines: &[Line<'_>],
    title_needle: &str,
    expected: Style,
) {
    let texts = line_texts(lines);
    assert_no_box_chars(&texts);
    let title = texts
        .iter()
        .position(|line| line.contains(title_needle))
        .unwrap_or_else(|| panic!("{label} missing artifact title: {texts:?}"));
    let title_style = lines[title]
        .spans
        .iter()
        .find(|span| {
            span.content.as_ref().contains(
                title_needle
                    .split_whitespace()
                    .next()
                    .unwrap_or(title_needle),
            )
        })
        .or_else(|| lines[title].spans.get(1))
        .or_else(|| lines[title].spans.first())
        .map(|span| span.style);
    assert_eq!(title_style, Some(expected), "{label} title: {texts:?}");
    for line in lines.iter().skip(title + 1).take_while(|line| {
        let text = line_text(line);
        // body rows have blank gutter + 2-space body pad, not hairlines
        text.starts_with("         ") && !text.contains('─')
    }) {
        let text = line_text(line);
        assert!(text.starts_with("         "), "{label} body row: {texts:?}");
        assert_eq!(
            line.style.bg,
            Some(expected.bg.expect("expected background")),
            "{label} body background: {texts:?}"
        );
    }
}

fn assert_no_box_chars(texts: &[String]) {
    let joined = texts.join("\n");
    // Warm Ledger allows tree ├/└ gutters; reject box *borders* only.
    assert!(
        !joined.contains(['┌', '┐', '┘', '│']),
        "box drawing leaked: {texts:?}"
    );
}

fn artifact_border_expected_style(theme: &Theme) -> Style {
    theme
        .transcript
        .tool
        .bg(theme.surfaces.transcript.background)
}

#[test]
fn projects_agent_spawn_message_result_into_companion_block() {
    let mut spawn = event(
        EventKind::AGENT_SPAWN,
        object([
            ("child_agent_id", "agent-child".into()),
            ("task", "review the patch".into()),
            ("persona", "reviewer".into()),
            ("provider", "fixture".into()),
            ("model", "echo".into()),
            ("capabilities", serde_json::json!([])),
            ("budget", serde_json::json!({})),
        ]),
    );
    spawn.id = "spawn-1".to_owned();
    spawn.ts = "2026-07-09T12:00:00.000Z".to_owned();

    let message = event(
        EventKind::AGENT_MESSAGE,
        object([
            ("from_agent_id", "agent-child".into()),
            ("to_agent_id", "agent".into()),
            ("spawn_event_id", "spawn-1".into()),
            ("queued_ts", "2026-07-09T12:00:30.000Z".into()),
            (
                "payload",
                serde_json::json!({"finding": "missing test", "severity": "high"}),
            ),
        ]),
    );

    let mut result = event(
        EventKind::AGENT_RESULT,
        object([
            ("child_agent_id", "agent-child".into()),
            ("spawn_event_id", "spawn-1".into()),
            ("ok", true.into()),
            ("summary", "review complete".into()),
            ("output", "ship it with a test".into()),
        ]),
    );
    result.ts = "2026-07-09T12:01:04.000Z".to_owned();

    let items = project_events(&[spawn, message, result]);
    assert_eq!(items.len(), 1, "items: {items:?}");
    match &items[0] {
        TranscriptItem::Companion {
            name,
            task,
            status,
            rows,
            spawn_event_id,
            ..
        } => {
            assert_eq!(spawn_event_id, "spawn-1");
            assert_eq!(name, "reviewer");
            assert_eq!(task, "review the patch");
            assert!(
                matches!(
                    status,
                    super::transcript::CompanionStatus::Done {
                        ok: true,
                        summary,
                        elapsed: Some(elapsed),
                    } if summary == "review complete" && elapsed == "1m 04s"
                ),
                "status: {status:?}"
            );
            assert!(
                rows.iter().any(|row| matches!(
                    row,
                    super::transcript::CompanionRow::Finding { label, detail }
                        if label == "high" && detail.contains("missing test")
                )),
                "rows: {rows:?}"
            );
            assert!(
                rows.iter().any(|row| matches!(
                    row,
                    super::transcript::CompanionRow::Report { text }
                        if text.contains("ship it")
                )),
                "rows: {rows:?}"
            );
        }
        other => panic!("expected Companion, got {other:?}"),
    }
}

#[test]
fn companion_block_collapses_by_default_and_expands_with_ctrl_o_key() {
    let item = TranscriptItem::Companion {
        spawn_event_id: "spawn-1".to_owned(),
        child_agent_id: "agent-child".to_owned(),
        name: "reviewer".to_owned(),
        task: "review".to_owned(),
        status: super::transcript::CompanionStatus::Done {
            ok: true,
            summary: "ok".to_owned(),
            elapsed: Some("1m 04s".to_owned()),
        },
        rows: vec![
            super::transcript::CompanionRow::Finding {
                label: "high".to_owned(),
                detail: "missing test".to_owned(),
            },
            super::transcript::CompanionRow::Report {
                text: "progress=50".to_owned(),
            },
        ],
    };
    assert!(item.is_foldable_artifact(DEFAULT_OUTPUT_LIMIT_LINES));

    let theme = Theme::default();
    let collapsed = line_texts(&render_items_for_history(
        std::slice::from_ref(&item),
        &theme,
        100,
    ))
    .join("\n");
    assert!(
        collapsed.contains("◆ reviewer · done 1m 04s · 1 findings"),
        "collapsed: {collapsed:?}"
    );
    assert!(
        collapsed.contains("ctrl+o expand"),
        "collapsed: {collapsed:?}"
    );
    assert!(
        !collapsed.contains("missing test"),
        "collapsed should hide findings: {collapsed:?}"
    );

    let mut expanded_keys = std::collections::HashSet::new();
    expanded_keys.insert(super::transcript::artifact_key_for_index(0));
    let expanded = line_texts(&super::transcript::render_items_for_history_with_expansion(
        &[item],
        &theme,
        100,
        DEFAULT_OUTPUT_LIMIT_LINES,
        &expanded_keys,
    ))
    .join("\n");
    assert!(expanded.contains("missing test"), "expanded: {expanded:?}");
    assert!(
        expanded.contains("ctrl+o collapse"),
        "expanded: {expanded:?}"
    );
}

#[test]
fn companion_running_header_and_finding_rows_use_teal_rail() {
    let item = TranscriptItem::Companion {
        spawn_event_id: "spawn-1".to_owned(),
        child_agent_id: "agent-child".to_owned(),
        name: "reviewer".to_owned(),
        task: "review the patch".to_owned(),
        status: super::transcript::CompanionStatus::Running,
        rows: vec![
            super::transcript::CompanionRow::Report {
                text: "older progress".to_owned(),
            },
            super::transcript::CompanionRow::Report {
                text: "mid progress".to_owned(),
            },
            super::transcript::CompanionRow::Finding {
                label: "high".to_owned(),
                detail: "race condition".to_owned(),
            },
        ],
    };
    let theme = Theme::default();
    let lines = render_items_for_history(&[item], &theme, 100);
    let joined = line_texts(&lines).join("\n");
    assert!(
        joined.contains("◆ reviewer ⠧ · review the patch"),
        "joined: {joined:?}"
    );
    assert!(
        joined.contains("own ledger · own permission scope"),
        "joined: {joined:?}"
    );
    assert!(
        joined.contains("… 1 earlier reports folded"),
        "joined: {joined:?}"
    );
    assert!(!joined.contains("older progress"), "joined: {joined:?}");
    assert!(joined.contains("mid progress"), "joined: {joined:?}");
    assert!(joined.contains("race condition"), "joined: {joined:?}");
    assert!(
        lines.iter().any(|line| {
            line.spans.iter().any(|span| {
                span.content.contains('\u{258c}') && span.style.fg == theme.transcript.companion.fg
            })
        }),
        "expected teal companion rail: {lines:?}"
    );
}

#[test]
fn child_agent_tool_events_are_suppressed_from_main_ledger() {
    let mut spawn = event(
        EventKind::AGENT_SPAWN,
        object([
            ("child_agent_id", "agent-child".into()),
            ("task", "review".into()),
            ("persona", "reviewer".into()),
            ("provider", "fixture".into()),
            ("model", "echo".into()),
            ("capabilities", serde_json::json!([])),
            ("budget", serde_json::json!({})),
        ]),
    );
    spawn.id = "spawn-1".to_owned();

    let mut child_tool = tool_call("call-1", "read_file", serde_json::json!({"path": "a.rs"}));
    child_tool.agent = "agent-child".to_owned();
    let mut child_result = event(
        EventKind::TOOL_RESULT,
        object([
            ("id", "call-1".into()),
            ("name", "read_file".into()),
            ("ok", true.into()),
            ("output", "fn main() {}".into()),
        ]),
    );
    child_result.agent = "agent-child".to_owned();

    let mut result = event(
        EventKind::AGENT_RESULT,
        object([
            ("child_agent_id", "agent-child".into()),
            ("spawn_event_id", "spawn-1".into()),
            ("ok", true.into()),
            ("summary", "done".into()),
        ]),
    );
    result.parent = Some("spawn-1".to_owned());

    let mut state = TranscriptState::default();
    for event in [spawn, child_tool, child_result, result] {
        state.push_event(event);
    }
    let items = state.items();
    assert!(
        items.iter().all(|item| !matches!(
            item,
            TranscriptItem::ToolCall { .. }
                | TranscriptItem::ToolResult { .. }
                | TranscriptItem::Exploration { .. }
                | TranscriptItem::ToolRun { .. }
        )),
        "child tools leaked: {items:?}"
    );
    assert!(
        items
            .iter()
            .any(|item| matches!(item, TranscriptItem::Companion { .. })),
        "missing companion: {items:?}"
    );
}

#[test]
fn concurrent_companions_remain_separate_blocks() {
    let mut spawn_a = event(
        EventKind::AGENT_SPAWN,
        object([
            ("child_agent_id", "child-a".into()),
            ("task", "task-a".into()),
            ("persona", "alpha".into()),
            ("provider", "fixture".into()),
            ("model", "echo".into()),
            ("capabilities", serde_json::json!([])),
            ("budget", serde_json::json!({})),
        ]),
    );
    spawn_a.id = "spawn-a".to_owned();
    let mut spawn_b = event(
        EventKind::AGENT_SPAWN,
        object([
            ("child_agent_id", "child-b".into()),
            ("task", "task-b".into()),
            ("persona", "beta".into()),
            ("provider", "fixture".into()),
            ("model", "echo".into()),
            ("capabilities", serde_json::json!([])),
            ("budget", serde_json::json!({})),
        ]),
    );
    spawn_b.id = "spawn-b".to_owned();
    let result_a = event(
        EventKind::AGENT_RESULT,
        object([
            ("child_agent_id", "child-a".into()),
            ("spawn_event_id", "spawn-a".into()),
            ("ok", true.into()),
            ("summary", "a done".into()),
        ]),
    );
    let result_b = event(
        EventKind::AGENT_RESULT,
        object([
            ("child_agent_id", "child-b".into()),
            ("spawn_event_id", "spawn-b".into()),
            ("ok", true.into()),
            ("summary", "b done".into()),
        ]),
    );

    let items = project_events(&[spawn_a, spawn_b, result_a, result_b]);
    assert_eq!(items.len(), 2, "items: {items:?}");
    match (&items[0], &items[1]) {
        (TranscriptItem::Companion { name: a, .. }, TranscriptItem::Companion { name: b, .. }) => {
            assert_eq!(a, "alpha");
            assert_eq!(b, "beta");
        }
        other => panic!("expected two companions: {other:?}"),
    }
}
