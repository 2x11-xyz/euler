use super::*;
use crate::ui::patch_approval::PatchPreview;
use crate::ui::test_backend::VT100Backend;
use euler_event::{object, EventEnvelope, EventKind};
use euler_provider::{
    catalog::{MergedModelCatalog, EMBEDDED_CATALOG_JSON},
    EchoProvider, FixtureResponse, ModelProvider, ModelRequest, ModelStreamEvent, ProviderError,
    ProviderStream, ReasoningEffort, ScriptedProvider, ScriptedStreamStep, StopReason, ToolCall,
    Usage,
};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    Terminal,
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Mutex;

mod chrome_tests;
mod live_memoization_tests;
mod permission_tests;

fn core() -> AppCore {
    core_with_provider(EchoProvider)
}

fn core_with_provider(provider: impl ModelProvider + 'static) -> AppCore {
    core_with_provider_model_at(provider, "echo", ".")
}

fn core_with_provider_at(
    provider: impl ModelProvider + 'static,
    root: impl Into<PathBuf>,
) -> AppCore {
    core_with_provider_model_at(provider, "echo", root)
}

fn core_with_provider_model_at(
    provider: impl ModelProvider + 'static,
    model: &str,
    root: impl Into<PathBuf>,
) -> AppCore {
    core_with_provider_model_options_at(provider, model, root, AppOptions::default())
}

fn core_with_provider_model_options_at(
    provider: impl ModelProvider + 'static,
    model: &str,
    root: impl Into<PathBuf>,
    options: AppOptions,
) -> AppCore {
    let (decider, channels) = TuiDecider::new();
    let mut config = euler_core::SessionConfig::new(root);
    config.model = model.to_owned();
    let session = Session::new(config, provider, decider);
    AppCore::new_with_options(session, channels, options)
}

fn core_with_fixture_catalog(
    provider: impl ModelProvider + 'static,
    model: &str,
    catalog: MergedModelCatalog,
) -> AppCore {
    core_with_provider_model_options_at(
        provider,
        model,
        ".",
        AppOptions {
            model_catalog: Some(catalog),
            ..AppOptions::default()
        },
    )
}

#[test]
fn background_catalog_refresh_reports_success_and_failure_without_blocking_ui() {
    let mut core = core();
    core.drain_finalized_visual_lines(80);
    let (success_tx, success_rx) = mpsc::channel();
    success_tx
        .send(Ok(crate::provider_catalog::RefreshReport {
            outcome: crate::provider_catalog::RefreshOutcome::Current {
                release_id: "catalog-v1-test".to_owned(),
            },
            warnings: Vec::new(),
        }))
        .expect("send success");
    core.catalog_refresh_rx = Some(success_rx);

    assert!(core.drain_background());
    assert!(drain_finalized_visual_text(&mut core, 80).contains("provider catalog is current"));

    let (failure_tx, failure_rx) = mpsc::channel();
    failure_tx
        .send(Err(anyhow!("offline")))
        .expect("send failure");
    core.catalog_refresh_rx = Some(failure_rx);

    assert!(core.drain_background());
    let text = drain_finalized_visual_text(&mut core, 80);
    assert!(text.contains("refresh unavailable"));
    assert!(text.contains("last-known-good models"));
}

#[test]
fn installed_catalog_reaches_idle_session_reasoning_policy() {
    let mut core = core_with_provider_model_at(ChatGptEchoProvider, "gpt-5.5", ".");
    let AppState::Idle { session } = &core.state else {
        panic!("test session must be idle");
    };
    assert_eq!(
        session
            .providers()
            .clamp_reasoning_effort("chatgpt", "gpt-5.5", ReasoningEffort::Max,),
        ReasoningEffort::XLarge
    );

    let updated = catalog_with_reasoning_efforts("chatgpt", "gpt-5.5", &["max"]);

    core.install_model_catalog(updated);

    let AppState::Idle { session } = &core.state else {
        panic!("test session must remain idle");
    };
    assert_eq!(
        session
            .providers()
            .clamp_reasoning_effort("chatgpt", "gpt-5.5", ReasoningEffort::Max,),
        ReasoningEffort::Max
    );
    assert_eq!(
        core.model_catalog
            .supported_reasoning_efforts("chatgpt", "gpt-5.5"),
        &[ReasoningEffort::Max]
    );
}

#[test]
fn installed_catalog_reaches_worker_session_at_the_turn_boundary() {
    let mut core = core_with_provider_model_at(ChatGptEchoProvider, "gpt-5.5", ".");
    let session = core.take_idle_session();
    let (_worker_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    core.install_model_catalog(catalog_with_reasoning_efforts(
        "chatgpt",
        "gpt-5.5",
        &["max"],
    ));

    // The worker keeps one coherent policy for its active turn.
    assert_eq!(
        session
            .providers()
            .clamp_reasoning_effort("chatgpt", "gpt-5.5", ReasoningEffort::Max,),
        ReasoningEffort::XLarge
    );

    core.handle_turn_event(TurnEvent::TurnDone {
        outcome: TurnOutcome::Complete,
        session,
    });

    let AppState::Idle { session } = &core.state else {
        panic!("returned worker session must be idle");
    };
    assert_eq!(
        session
            .providers()
            .clamp_reasoning_effort("chatgpt", "gpt-5.5", ReasoningEffort::Max,),
        ReasoningEffort::Max
    );
}

fn catalog_with_reasoning_efforts(
    provider: &str,
    model_id: &str,
    efforts: &[&str],
) -> MergedModelCatalog {
    let mut document: serde_json::Value =
        serde_json::from_str(EMBEDDED_CATALOG_JSON).expect("embedded catalog");
    let model = document["providers"][provider]["models"]
        .as_array_mut()
        .expect("provider models")
        .iter_mut()
        .find(|model| model["id"] == model_id)
        .expect("catalog model");
    model["reasoning_efforts"] = json!(efforts);
    MergedModelCatalog::from_official_json(
        &serde_json::to_string(&document).expect("updated catalog JSON"),
    )
    .expect("updated catalog")
}

fn fixture_catalog_with_windows(models: &[(&str, u64)]) -> MergedModelCatalog {
    let descriptors = models
        .iter()
        .map(|(id, window)| json!({"id": id, "context_window_tokens": window}))
        .collect::<Vec<_>>();
    let (catalog, warnings) = MergedModelCatalog::with_local_json(
        &json!({"providers": {"fixture": {"models": descriptors}}}).to_string(),
    );
    assert!(warnings.is_empty(), "catalog warnings: {warnings:?}");
    catalog
}

fn scripted_usage(input_tokens: u64) -> FixtureResponse {
    FixtureResponse::Stream(vec![ScriptedStreamStep::Event(
        ModelStreamEvent::Finished {
            stop_reason: StopReason::Completed,
            usage: Some(Usage {
                input_tokens,
                output_tokens: 999,
                uncached_input_tokens: Some(input_tokens),
                cached_tokens: Some(0),
                cache_write_5m_tokens: Some(0),
                cache_write_1h_tokens: Some(0),
                reasoning_tokens: Some(500),
            }),
        },
    )])
}

#[test]
fn ratatui_to_canvas_preserves_span_styles() {
    let lines = vec![Line::from(vec![
        Span::styled(
            "warn",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" plain"),
    ])
    .style(Style::default().bg(Color::Blue))];

    let canvas = ratatui_lines_to_canvas(lines);

    assert_eq!(canvas[0].plain_text(), "warn plain");
    assert_eq!(canvas[0].spans[0].style.fg, Some(Color::Yellow));
    assert_eq!(canvas[0].spans[0].style.bg, Some(Color::Blue));
    assert!(canvas[0].spans[0]
        .style
        .add_modifier
        .contains(Modifier::BOLD));
    assert_eq!(canvas[0].spans[1].style.bg, Some(Color::Blue));
}

struct SlowEchoProvider;

impl ModelProvider for SlowEchoProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        std::thread::sleep(Duration::from_millis(50));
        EchoProvider.invoke(request)
    }
}

struct ChatGptEchoProvider;

impl ModelProvider for ChatGptEchoProvider {
    fn name(&self) -> &'static str {
        "chatgpt"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        EchoProvider.invoke(request)
    }
}

struct MockEditor {
    result: EditorResult,
}

impl ExternalEditorRunner for MockEditor {
    fn edit(&self, _draft: &str) -> EditorResult {
        self.result.clone()
    }
}

struct CapturingEditor {
    drafts: Arc<Mutex<Vec<String>>>,
    result: EditorResult,
}

impl ExternalEditorRunner for CapturingEditor {
    fn edit(&self, draft: &str) -> EditorResult {
        self.drafts
            .lock()
            .expect("editor drafts lock")
            .push(draft.to_owned());
        self.result.clone()
    }
}

#[derive(Clone)]
struct RecordingClipboard {
    writes: Arc<Mutex<Vec<String>>>,
    result: Result<(), String>,
}

impl ClipboardSink for RecordingClipboard {
    fn copy(&self, text: &str) -> Result<(), String> {
        self.writes
            .lock()
            .expect("clipboard lock")
            .push(text.to_owned());
        self.result.clone()
    }
}

fn key(code: KeyCode) -> InputEvent {
    InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> InputEvent {
    InputEvent::Key(KeyEvent::new(code, modifiers))
}

fn ctrl_o() -> InputEvent {
    modified_key(KeyCode::Char('o'), KeyModifiers::CONTROL)
}

fn type_text(core: &mut AppCore, text: &str) {
    for ch in text.chars() {
        if ch == '\n' {
            core.handle_input(modified_key(KeyCode::Enter, KeyModifiers::SHIFT));
        } else {
            core.handle_input(key(KeyCode::Char(ch)));
        }
    }
}

fn submit_text_and_wait(core: &mut AppCore, text: &str) {
    type_text(core, text);
    core.handle_input(key(KeyCode::Enter));
    wait_for_idle(core);
}

fn submit_without_wait(core: &mut AppCore, text: &str) {
    type_text(core, text);
    core.handle_input(key(KeyCode::Enter));
}

fn user_messages(core: &AppCore) -> Vec<String> {
    core.transcript
        .events()
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::USER_MESSAGE)
        .filter_map(|event| event.payload.get("content")?.as_str().map(str::to_owned))
        .collect()
}

fn shell_artifact_with_lines(total: usize) -> TranscriptItem {
    TranscriptItem::ToolRun {
        command: "printf lines".to_owned(),
        ok: true,
        error: String::new(),
        output: (1..=total)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n"),
        exit_code: Some(0),
        grant_source: None,
        static_safe: false,
    }
}

fn mouse_event(kind: MouseEventKind) -> MouseEvent {
    MouseEvent {
        kind,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    }
}

fn screen_row(contents: &str, row: u16) -> &str {
    contents.lines().nth(usize::from(row)).unwrap_or("")
}

fn fs_write_request() -> PermissionRequest {
    PermissionRequest::new(Capability::FsWrite, "tool edit_file".to_owned())
}

fn apply_patch_request() -> PermissionRequest {
    PermissionRequest::new(Capability::FsWrite, "tool apply_patch".to_owned())
}

fn patch_modal(preview: PatchPreview) -> Modal {
    Modal::PatchApproval(PatchApprovalModal {
        request: fs_write_request(),
        preview,
    })
}

fn diff_preview(old: &str, new: &str) -> PatchPreview {
    PatchPreview::Diff {
        path: "note.txt".to_owned(),
        old: old.to_owned(),
        new: new.to_owned(),
    }
}

#[test]
fn submit_starts_in_flight_and_second_submit_queues() {
    let mut core = core();
    core.handle_input(key(KeyCode::Char('h')));
    core.handle_input(key(KeyCode::Enter));
    assert!(core.turn_in_flight());

    core.handle_input(key(KeyCode::Char('q')));
    core.handle_input(key(KeyCode::Enter));

    assert!(core.notice.is_none());
    assert_eq!(core.queued_inputs.snapshot(), ["q"]);
    assert_eq!(core.bottom.composer().submit_text(), "");
}

#[test]
fn queued_steer_preview_is_visual_only() {
    let full = "Right but don't mention anywhere in any doc that we're not mentioning other services or other products.";
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    core.queued_inputs.push_back(full.to_owned());

    let queued_line = core
        .visual_canvas_frame(120)
        .active_frame_lines()
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .find(|line| line.starts_with("▌ 1/1 "))
        .expect("queued steer row");

    assert_eq!(
        queued_line,
        "▌ 1/1 Right but don't mention anywhere in any doc that we're not ..."
    );
    assert_eq!(core.queued_inputs.snapshot(), [full]);
}

#[test]
fn queued_inputs_auto_flush_fifo_after_normal_completion() {
    let mut core = core_with_provider(SlowEchoProvider);
    submit_without_wait(&mut core, "first");
    submit_without_wait(&mut core, "second");
    submit_without_wait(&mut core, "third");

    wait_for_idle(&mut core);

    // FIFO holds whether the trailing submissions steered the running turn
    // (absorbed at its first boundary) or flushed as their own turns —
    // front-only absorption makes overtaking impossible either way. The
    // strict per-turn interleave is asserted deterministically in
    // `queued_leftovers_run_as_their_own_turns`.
    assert_eq!(user_messages(&core), ["first", "second", "third"]);
    assert!(core.queued_inputs.is_empty());
}

#[test]
fn queued_leftovers_run_as_their_own_turns() {
    // Review blocker (PR #147): leftovers queued BEFORE a turn spawns must
    // never fold into that turn's request. Queued directly while idle, the
    // entries predate the first spawn's steering generation, so each must
    // flush as its own turn: user → its model.call, three times.
    let mut core = core_with_provider(SlowEchoProvider);
    core.queued_inputs.push_back("second".to_owned());
    core.queued_inputs.push_back("third".to_owned());
    submit_without_wait(&mut core, "first");

    wait_for_idle(&mut core);

    assert_eq!(user_messages(&core), ["first", "second", "third"]);
    assert!(core.queued_inputs.is_empty());
    let ordered: Vec<&str> = core
        .transcript
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event.kind.as_str(),
                EventKind::USER_MESSAGE | EventKind::MODEL_CALL
            )
        })
        .map(|event| event.kind.as_str())
        .collect();
    assert_eq!(
        ordered,
        [
            EventKind::USER_MESSAGE,
            EventKind::MODEL_CALL,
            EventKind::USER_MESSAGE,
            EventKind::MODEL_CALL,
            EventKind::USER_MESSAGE,
            EventKind::MODEL_CALL,
        ]
    );
}

#[test]
fn interrupt_keeps_queue_until_user_continues() {
    let mut core = core_with_provider(SlowEchoProvider);
    submit_without_wait(&mut core, "first");
    submit_without_wait(&mut core, "queued");

    core.handle_input(key(KeyCode::Esc));
    wait_for_idle(&mut core);

    assert_eq!(core.queued_inputs.snapshot(), ["queued"]);
    assert_eq!(user_messages(&core), ["first"]);

    core.handle_input(key(KeyCode::Enter));
    wait_for_idle(&mut core);

    assert_eq!(user_messages(&core), ["first", "queued"]);
}

#[test]
fn queued_input_recall_and_unqueue_use_selected_or_last() {
    let mut core = core_with_provider(SlowEchoProvider);
    submit_without_wait(&mut core, "active");
    submit_without_wait(&mut core, "one");
    submit_without_wait(&mut core, "two");

    assert_eq!(core.handle_input(key(KeyCode::Up)), CoreEffect::Render);
    assert_eq!(core.bottom.composer().submit_text(), "two");
    assert_eq!(core.queued_inputs.snapshot(), ["one"]);

    core.handle_input(key(KeyCode::Enter));
    type_text(&mut core, "three");
    core.handle_input(key(KeyCode::Enter));
    assert_eq!(core.queued_inputs.snapshot(), ["one", "two", "three"]);

    core.handle_input(key(KeyCode::Left));
    assert_eq!(
        core.handle_input(modified_key(KeyCode::Char('u'), KeyModifiers::CONTROL)),
        CoreEffect::Render
    );
    assert_eq!(core.queued_inputs.snapshot(), ["one", "three"]);
}

#[test]
fn worker_completion_returns_idle() {
    let mut core = core();
    core.handle_input(key(KeyCode::Char('h')));
    core.handle_input(key(KeyCode::Enter));

    for _ in 0..50 {
        if core.drain_background() && !core.turn_in_flight() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(!core.turn_in_flight());
    assert!(core
        .transcript
        .events()
        .iter()
        .any(|event| event.kind.as_str() == euler_event::EventKind::ASSISTANT_MESSAGE));
}

#[test]
fn modal_input_replies_allow_deny_and_scoped() {
    for (code, reply) in [
        (KeyCode::Char('y'), PermissionReply::AllowOnce),
        (KeyCode::Char('n'), PermissionReply::Deny),
        (
            KeyCode::Char('a'),
            PermissionReply::AllowSessionScope(String::new()),
        ),
        (
            KeyCode::Char('p'),
            PermissionReply::AllowProjectScope(String::new()),
        ),
    ] {
        let mut core = core();
        let (reply_tx, reply_rx) = mpsc::channel();
        core.reply_tx = reply_tx;
        core.modal = Some(Modal::Permission(PermissionRequest::new(
            Capability::FsWrite,
            "edit".to_owned(),
        )));
        core.handle_input(key(code));
        assert_eq!(reply_rx.recv().expect("reply"), reply);
    }
}

#[test]
fn patch_modal_session_scope_uses_path_prefix_when_present() {
    let mut core = core();
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.modal = Some(Modal::PatchApproval(PatchApprovalModal {
        request: fs_write_request().with_path("crates/euler-cli/src/lib.rs"),
        preview: diff_preview("alpha\n", "beta\n"),
    }));

    let effect = core.handle_input(key(KeyCode::Char('a')));

    assert_eq!(effect, CoreEffect::Render);
    assert!(core.modal.is_none());
    assert_eq!(
        reply_rx.recv().expect("reply"),
        PermissionReply::AllowSessionScope("crates".into())
    );
}

#[test]
fn modal_ctrl_c_defers_deny_until_shutdown_preparation() {
    let mut core = core();
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.modal = Some(Modal::Permission(PermissionRequest::new(
        Capability::FsWrite,
        "edit".to_owned(),
    )));

    let effect = core.handle_input(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert_eq!(effect, CoreEffect::Quit);
    assert!(matches!(core.modal, Some(Modal::Permission(_))));
    assert!(reply_rx.try_recv().is_err());

    core.prepare_for_shutdown();

    assert!(core.modal.is_none());
    assert_eq!(reply_rx.recv().expect("reply"), PermissionReply::Deny);
}

#[test]
fn patch_modal_ctrl_c_or_d_defers_deny_until_shutdown_preparation() {
    for code in [
        KeyCode::Char('c'),
        KeyCode::Char('C'),
        KeyCode::Char('d'),
        KeyCode::Char('D'),
    ] {
        let mut core = core();
        let (reply_tx, reply_rx) = mpsc::channel();
        core.reply_tx = reply_tx;
        core.modal = Some(patch_modal(diff_preview("alpha\n", "beta\n")));

        let effect = core.handle_input(modified_key(code, KeyModifiers::CONTROL));

        assert_eq!(effect, CoreEffect::Quit);
        assert!(matches!(core.modal, Some(Modal::PatchApproval(_))));
        assert!(reply_rx.try_recv().is_err());

        core.prepare_for_shutdown();

        assert!(core.modal.is_none());
        assert_eq!(reply_rx.recv().expect("reply"), PermissionReply::Deny);
    }
}

#[test]
fn patch_modal_ctrl_shift_c_does_not_deny_or_quit() {
    let mut core = core();
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.modal = Some(patch_modal(diff_preview("alpha\n", "beta\n")));

    let effect = core.handle_input(modified_key(
        KeyCode::Char('C'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));

    assert_eq!(effect, CoreEffect::None);
    assert!(matches!(core.modal, Some(Modal::PatchApproval(_))));
    assert!(reply_rx.try_recv().is_err());
}

#[test]
fn permission_modal_swallows_scrollback_keys_without_replying() {
    let mut core = core();
    let (reply_tx, reply_rx) = mpsc::channel();
    core.reply_tx = reply_tx;
    core.modal = Some(Modal::Permission(PermissionRequest::new(
        Capability::FsWrite,
        "edit".to_owned(),
    )));

    // PageUp/Ctrl+Up may move selection (Render) or be swallowed (None), but
    // must never send a permission reply.
    let page_up = core.handle_input(key(KeyCode::PageUp));
    assert!(
        matches!(page_up, CoreEffect::None | CoreEffect::Render),
        "page up should not reply: {page_up:?}"
    );
    let ctrl_up = core.handle_input(modified_key(KeyCode::Up, KeyModifiers::CONTROL));
    assert!(
        matches!(ctrl_up, CoreEffect::None | CoreEffect::Render),
        "ctrl+up should not reply: {ctrl_up:?}"
    );

    assert_eq!(core.transcript.scroll_offset(), 0);
    assert!(matches!(core.modal, Some(Modal::Permission(_))));
    assert!(reply_rx.try_recv().is_err());
}

#[test]
fn patch_modal_ctrl_x_does_not_launch_editor_or_close_modal() {
    let mut core = core();
    let drafts = Arc::new(Mutex::new(Vec::new()));
    core.editor = Box::new(CapturingEditor {
        drafts: Arc::clone(&drafts),
        result: EditorResult::Updated("should not apply".to_owned()),
    });
    core.modal = Some(patch_modal(diff_preview("alpha\n", "beta\n")));

    assert_eq!(
        core.handle_input(modified_key(KeyCode::Char('x'), KeyModifiers::CONTROL)),
        CoreEffect::None
    );

    assert!(drafts.lock().expect("editor drafts lock").is_empty());
    assert!(matches!(core.modal, Some(Modal::PatchApproval(_))));
}

#[test]
fn multiline_bracketed_paste_inserts_paste_token_and_waits_for_enter() {
    let mut core = core();
    let payload = (1..=11)
        .map(|line| format!("line{line}"))
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(
        core.handle_input(InputEvent::Paste(payload.clone())),
        CoreEffect::Render
    );

    assert_eq!(core.bottom.composer().render_text(), "[paste #1 +11 lines]");
    assert_eq!(core.bottom.composer().submit_text(), payload);
    assert!(!core.turn_in_flight());

    assert_eq!(core.handle_input(key(KeyCode::Enter)), CoreEffect::Render);
    assert!(core.turn_in_flight());
    wait_for_idle(&mut core);

    assert!(core.transcript.events().iter().any(|event| {
        event.kind.as_str() == EventKind::USER_MESSAGE
            && event
                .payload
                .get("content")
                .and_then(serde_json::Value::as_str)
                == Some(payload.as_str())
    }));
}

#[test]
fn shift_enter_inserts_newline_without_submitting() {
    let mut core = core();
    core.handle_input(key(KeyCode::Char('a')));
    core.handle_input(modified_key(KeyCode::Enter, KeyModifiers::SHIFT));
    core.handle_input(key(KeyCode::Char('b')));

    assert_eq!(core.bottom.composer().submit_text(), "a\nb");
    assert!(!core.turn_in_flight());
}

#[test]
fn alt_enter_fallback_inserts_newline_without_submitting() {
    let mut core = core();
    core.handle_input(key(KeyCode::Char('a')));
    core.handle_input(modified_key(KeyCode::Enter, KeyModifiers::ALT));
    core.handle_input(key(KeyCode::Char('b')));

    assert_eq!(core.bottom.composer().submit_text(), "a\nb");
    assert!(!core.turn_in_flight());
}

#[test]
fn composer_history_recalls_oldest_then_down_restores_draft() {
    let mut core = core();
    submit_text_and_wait(&mut core, "first");
    submit_text_and_wait(&mut core, "second");
    type_text(&mut core, "draft kept");

    core.handle_input(key(KeyCode::Up));
    assert_eq!(core.bottom.composer().submit_text(), "second");
    core.handle_input(key(KeyCode::Up));
    assert_eq!(core.bottom.composer().submit_text(), "first");
    core.handle_input(key(KeyCode::Up));
    assert_eq!(core.bottom.composer().submit_text(), "first");
    core.handle_input(key(KeyCode::Down));
    assert_eq!(core.bottom.composer().submit_text(), "second");
    core.handle_input(key(KeyCode::Down));
    assert_eq!(core.bottom.composer().submit_text(), "draft kept");
}

#[test]
fn composer_history_multiline_up_moves_before_recalling() {
    let mut core = core();
    submit_text_and_wait(&mut core, "remembered");
    type_text(&mut core, "line1\nline2");

    core.handle_input(key(KeyCode::Up));

    assert_eq!(core.bottom.composer().submit_text(), "line1\nline2");
    core.handle_input(key(KeyCode::Up));
    assert_eq!(core.bottom.composer().submit_text(), "remembered");
}

#[test]
fn composer_history_submitting_recalled_entry_makes_it_newest() {
    let mut core = core();
    submit_text_and_wait(&mut core, "alpha");
    submit_text_and_wait(&mut core, "beta");
    core.handle_input(key(KeyCode::Up));
    core.handle_input(key(KeyCode::Up));
    assert_eq!(core.bottom.composer().submit_text(), "alpha");

    core.handle_input(key(KeyCode::Enter));
    wait_for_idle(&mut core);
    type_text(&mut core, "draft");
    core.handle_input(key(KeyCode::Up));

    assert_eq!(core.bottom.composer().submit_text(), "alpha");
}

#[test]
fn composer_history_uses_visual_rows_for_up_boundary() {
    let mut core = core();
    submit_text_and_wait(&mut core, "remembered");
    type_text(&mut core, "abcdefghij");
    let _ = core.visual_canvas_frame(7);

    core.handle_input(key(KeyCode::Up));

    assert_eq!(core.bottom.composer().submit_text(), "abcdefghij");
    core.handle_input(key(KeyCode::Up));
    assert_eq!(core.bottom.composer().submit_text(), "remembered");
}

#[test]
fn composer_history_edit_detaches_and_submission_filtering_is_honest() {
    let mut core = core();
    submit_text_and_wait(&mut core, "one");
    submit_text_and_wait(&mut core, "one");
    assert_eq!(core.handle_input(key(KeyCode::Enter)), CoreEffect::None);
    submit_text_and_wait(&mut core, "two");

    core.handle_input(key(KeyCode::Up));
    assert_eq!(core.bottom.composer().submit_text(), "two");
    core.handle_input(key(KeyCode::Up));
    assert_eq!(core.bottom.composer().submit_text(), "one");
    core.handle_input(key(KeyCode::Down));
    assert_eq!(core.bottom.composer().submit_text(), "two");

    core.handle_input(key(KeyCode::Char('!')));
    assert_eq!(core.bottom.composer().submit_text(), "two!");
    core.handle_input(key(KeyCode::Down));
    assert_eq!(core.bottom.composer().submit_text(), "two!");
    core.handle_input(key(KeyCode::Up));
    assert_eq!(core.bottom.composer().submit_text(), "two");
    core.handle_input(key(KeyCode::Down));
    assert_eq!(core.bottom.composer().submit_text(), "two!");
}

#[test]
fn ctrl_c_double_press_quits_when_idle() {
    let mut core = core();
    let ctrl_c = modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL);

    assert_eq!(core.handle_input(ctrl_c.clone()), CoreEffect::Render);
    assert_eq!(
        core.notice.as_deref(),
        Some("ctrl+c again to quit · session saved, /resume restores")
    );
    assert_eq!(core.handle_input(ctrl_c), CoreEffect::Quit);
}

#[test]
fn terminal_interrupt_double_press_quits_when_idle() {
    let mut core = core();

    assert_eq!(core.handle_terminal_interrupt(), CoreEffect::Render);
    assert_eq!(core.handle_terminal_interrupt(), CoreEffect::Quit);
}

#[test]
fn uppercase_ctrl_c_without_shift_arms_quit_and_does_not_copy() {
    let mut core = core();
    let writes = Arc::new(Mutex::new(Vec::new()));
    core.clipboard = Box::new(RecordingClipboard {
        writes: Arc::clone(&writes),
        result: Ok(()),
    });
    core.transcript.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "do not copy".into())]),
    ));

    assert_eq!(
        core.handle_input(modified_key(KeyCode::Char('C'), KeyModifiers::CONTROL)),
        CoreEffect::Render
    );

    assert!(writes.lock().expect("clipboard lock").is_empty());
    assert_eq!(
        core.notice.as_deref(),
        Some("ctrl+c again to quit · session saved, /resume restores")
    );
}

#[test]
fn escape_interrupts_in_flight_turn() {
    let mut core = core_with_provider(SlowEchoProvider);
    core.handle_input(key(KeyCode::Char('h')));
    core.handle_input(key(KeyCode::Enter));

    assert_eq!(core.handle_input(key(KeyCode::Esc)), CoreEffect::Render);
    assert!(core.notice.is_none());
    assert!(core.interrupted_guidance);
    let AppState::TurnInFlight { interrupt_flag, .. } = &core.state else {
        panic!("turn should still be in flight");
    };
    assert!(interrupt_flag.load(Ordering::SeqCst));

    for _ in 0..50 {
        if core.drain_background() && !core.turn_in_flight() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(core.notice.is_none());
    assert!(!core.interrupted_guidance);
    assert!(drain_finalized_visual_text(&mut core, 80)
        .contains("interrupted — tell euler what to do differently"));
}

#[test]
fn shutdown_cancellation_sets_in_flight_turn_interrupt_flag() {
    // Deep-review P3-d: /quit mid-turn publishes the same cancel signal the
    // esc interrupt uses so the detached worker's provider round stops
    // promptly instead of running until process exit.
    let mut core = core_with_provider(SlowEchoProvider);
    core.handle_input(key(KeyCode::Char('h')));
    core.handle_input(key(KeyCode::Enter));
    assert!(core.turn_in_flight());

    core.cancel_in_flight_for_shutdown();

    // Pause-before-flag, same ordering contract as `handle_interrupt`.
    assert!(core.queued_inputs.paused());
    let AppState::TurnInFlight { interrupt_flag, .. } = &core.state else {
        panic!("turn should still be in flight");
    };
    assert!(interrupt_flag.load(Ordering::SeqCst));

    // The worker observes the flag and returns the session; drain so the
    // thread finishes within the test.
    for _ in 0..50 {
        if core.drain_background() && !core.turn_in_flight() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!core.turn_in_flight());
}

#[test]
fn shutdown_cancels_before_releasing_permission_modal() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-write".to_owned(),
            name: "run_shell".to_owned(),
            input: json!({"command": "printf should-not-run > shutdown-marker.txt"}),
        }]),
        FixtureResponse::Assistant("must not continue after shutdown".to_owned()),
    ]);
    let mut core = core_with_provider_at(provider, temp.path());
    core.handle_input(key(KeyCode::Char('h')));
    core.handle_input(key(KeyCode::Enter));
    wait_for_permission_modal(&mut core);
    let interrupt_flag = match &core.state {
        AppState::TurnInFlight { interrupt_flag, .. } => Arc::clone(interrupt_flag),
        _ => panic!("turn should be in flight"),
    };

    let effect = core.handle_input(modified_key(KeyCode::Char('d'), KeyModifiers::CONTROL));

    assert_eq!(effect, CoreEffect::Quit);
    std::thread::sleep(Duration::from_millis(20));
    core.drain_background();
    assert!(core.turn_in_flight(), "quit intent must not wake worker");
    assert!(matches!(core.modal, Some(Modal::Permission(_))));
    assert!(!core.queued_inputs.paused());
    assert!(!interrupt_flag.load(Ordering::SeqCst));

    core.prepare_for_shutdown();

    assert!(core.queued_inputs.paused());
    assert!(interrupt_flag.load(Ordering::SeqCst));
    assert!(core.modal.is_none());

    for _ in 0..100 {
        if core.drain_background() && !core.turn_in_flight() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!core.turn_in_flight());
    assert!(!temp.path().join("shutdown-marker.txt").exists());
    assert!(!core.transcript.items().iter().any(|item| {
        matches!(
            item,
            TranscriptItem::AssistantMessage(content)
                if content == "must not continue after shutdown"
        )
    }));
}

#[test]
fn shutdown_cancellation_is_a_no_op_when_idle() {
    let mut core = core();
    core.cancel_in_flight_for_shutdown();
    assert!(matches!(core.state, AppState::Idle { .. }));
    assert!(!core.queued_inputs.paused());
}

#[test]
#[should_panic(expected = "worker-channel invariant violated")]
fn replacing_turn_in_flight_without_terminal_event_is_diagnosed() {
    // Deep-review P3-e: the live session rides the worker channel and comes
    // back only through the TurnInFlight receiver's terminal event, so
    // replacing that state any other way must be loudly diagnosed.
    let mut core = core();
    let _session = core.take_idle_session();
    let (_worker_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    core.install_state(AppState::Empty);
}

#[test]
fn turn_done_licenses_exactly_one_turn_in_flight_replacement() {
    // The sanctioned path — consuming TurnDone — replaces TurnInFlight with
    // Idle without tripping the guard, and the license does not linger.
    let mut core = core();
    let session = core.take_idle_session();
    let (_worker_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    core.handle_turn_event(TurnEvent::TurnDone {
        outcome: TurnOutcome::Complete,
        session,
    });

    assert!(matches!(core.state, AppState::Idle { .. }));
    assert!(!core.in_flight_session_returned);
}

#[test]
fn uppercase_ctrl_c_without_shift_interrupts_in_flight_and_does_not_copy() {
    let mut core = core();
    let writes = Arc::new(Mutex::new(Vec::new()));
    let (_tx, worker_rx) = mpsc::channel();
    let interrupt_flag = Arc::new(AtomicBool::new(false));
    core.clipboard = Box::new(RecordingClipboard {
        writes: Arc::clone(&writes),
        result: Ok(()),
    });
    core.transcript.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "do not copy".into())]),
    ));
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::clone(&interrupt_flag),
        started_at: Instant::now(),
    };

    assert_eq!(
        core.handle_input(modified_key(KeyCode::Char('C'), KeyModifiers::CONTROL)),
        CoreEffect::Render
    );

    assert!(writes.lock().expect("clipboard lock").is_empty());
    assert!(interrupt_flag.load(Ordering::SeqCst));
    assert!(core.notice.is_none());
    assert!(core.interrupted_guidance);
}

#[test]
fn repeated_ctrl_c_after_active_turn_interrupt_quits() {
    let mut core = core();
    let interrupt_flag = Arc::new(AtomicBool::new(false));
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::clone(&interrupt_flag),
        started_at: Instant::now(),
    };
    let ctrl_c = modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL);

    assert_eq!(core.handle_input(ctrl_c.clone()), CoreEffect::Render);
    assert!(interrupt_flag.load(Ordering::SeqCst));
    assert!(core.interrupted_guidance);
    assert_eq!(core.handle_input(ctrl_c), CoreEffect::Quit);
}

#[test]
fn ctrl_d_quits_during_interrupted_active_turn_when_next_draft_is_empty() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    core.interrupted_guidance = true;

    assert_eq!(
        core.handle_input(modified_key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
        CoreEffect::Quit
    );
}

#[test]
fn transcript_review_keys_move_visual_canvas_without_transcript_widget_offset() {
    let mut core = core();

    assert_eq!(core.handle_input(key(KeyCode::PageUp)), CoreEffect::Render);
    assert_eq!(core.visual_scroll_offset(), 8);
    assert_eq!(
        core.handle_input(modified_key(KeyCode::Up, KeyModifiers::CONTROL)),
        CoreEffect::Render
    );
    assert_eq!(core.visual_scroll_offset(), 9);
    assert_eq!(
        core.handle_input(modified_key(KeyCode::Down, KeyModifiers::CONTROL)),
        CoreEffect::Render
    );
    assert_eq!(core.visual_scroll_offset(), 8);
    assert_eq!(
        core.handle_input(key(KeyCode::PageDown)),
        CoreEffect::Render
    );
    assert_eq!(core.visual_scroll_offset(), 0);

    assert_eq!(core.transcript.scroll_offset(), 0);
    assert!(core.transcript.auto_follow());
}

#[test]
fn transcript_review_keys_work_while_turn_is_in_flight() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    assert_eq!(core.handle_input(key(KeyCode::PageUp)), CoreEffect::Render);
    assert_eq!(core.visual_scroll_offset(), 8);
    assert_eq!(
        core.handle_input(key(KeyCode::PageDown)),
        CoreEffect::Render
    );
    assert_eq!(core.visual_scroll_offset(), 0);
}

#[test]
fn mouse_wheel_reviews_visual_canvas_while_turn_is_in_flight() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    assert_eq!(
        core.handle_input(InputEvent::Mouse(mouse_event(MouseEventKind::ScrollUp))),
        CoreEffect::Render
    );
    assert_eq!(core.visual_scroll_offset(), 3);
    assert_eq!(
        core.handle_input(InputEvent::Mouse(mouse_event(MouseEventKind::ScrollDown))),
        CoreEffect::Render
    );
    assert_eq!(core.visual_scroll_offset(), 0);
}

#[test]
fn mouse_wheel_does_not_scroll_behind_modal() {
    let mut core = core();
    core.modal = Some(Modal::Permission(fs_write_request()));

    assert_eq!(
        core.handle_input(InputEvent::Mouse(mouse_event(MouseEventKind::ScrollUp))),
        CoreEffect::None
    );

    assert_eq!(core.visual_scroll_offset(), 0);
    assert!(matches!(core.modal, Some(Modal::Permission(_))));
}

#[test]
fn non_wheel_mouse_events_are_not_semantic_ui_input() {
    let mut core = core();

    assert_eq!(
        core.handle_input(InputEvent::Mouse(mouse_event(MouseEventKind::Down(
            crossterm::event::MouseButton::Left
        )))),
        CoreEffect::None
    );
    assert_eq!(core.visual_scroll_offset(), 0);
    assert!(core.bottom.composer().submit_text().is_empty());
}

#[test]
fn composer_accepts_next_draft_edits_while_turn_is_in_flight() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    for ch in "next".chars() {
        assert_eq!(
            core.handle_input(key(KeyCode::Char(ch))),
            CoreEffect::Render
        );
    }
    assert_eq!(
        core.handle_input(modified_key(KeyCode::Enter, KeyModifiers::SHIFT)),
        CoreEffect::Render
    );
    for ch in "draft".chars() {
        assert_eq!(
            core.handle_input(key(KeyCode::Char(ch))),
            CoreEffect::Render
        );
    }

    assert_eq!(core.bottom.composer().submit_text(), "next\ndraft");
    assert!(core.turn_in_flight());
    assert_eq!(core.handle_input(key(KeyCode::Enter)), CoreEffect::Render);
    assert!(core.notice.is_none());
    assert_eq!(core.queued_inputs.snapshot(), ["next\ndraft"]);
    assert_eq!(core.bottom.composer().submit_text(), "");
}

#[test]
fn active_turn_draft_ignores_command_modified_text_keys() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    assert_eq!(
        core.handle_input(modified_key(KeyCode::Char('A'), KeyModifiers::SHIFT)),
        CoreEffect::Render
    );
    assert_eq!(
        core.handle_input(modified_key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
        CoreEffect::None
    );
    assert_eq!(
        core.handle_input(modified_key(KeyCode::Char('x'), KeyModifiers::ALT)),
        CoreEffect::None
    );
    assert_eq!(
        core.handle_input(modified_key(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )),
        CoreEffect::Render
    );

    assert_eq!(core.bottom.composer().submit_text(), "A@");
    assert!(core.turn_in_flight());
}

#[test]
fn bracketed_paste_updates_next_draft_while_turn_is_in_flight() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    assert_eq!(
        core.handle_input(InputEvent::Paste("pasted\nlines".to_owned())),
        CoreEffect::Render
    );

    assert_eq!(core.bottom.composer().submit_text(), "pasted\nlines");
    assert!(core.turn_in_flight());
}

#[test]
fn slash_opens_palette_while_turn_is_in_flight_when_next_draft_is_empty() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    assert_eq!(
        core.handle_input(key(KeyCode::Char('/'))),
        CoreEffect::Render
    );

    assert!(matches!(core.bottom.owner(), BottomOwner::Palette(_)));
    assert_eq!(core.bottom.composer().submit_text(), "");
}

#[test]
fn shifted_slash_opens_palette_while_turn_is_in_flight_when_next_draft_is_empty() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    assert_eq!(
        core.handle_input(modified_key(KeyCode::Char('/'), KeyModifiers::SHIFT)),
        CoreEffect::Render
    );

    assert!(matches!(core.bottom.owner(), BottomOwner::Palette(_)));
    assert_eq!(core.bottom.composer().submit_text(), "");
}

#[test]
fn control_slash_does_not_open_palette_while_turn_is_in_flight() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    assert_eq!(
        core.handle_input(modified_key(KeyCode::Char('/'), KeyModifiers::CONTROL)),
        CoreEffect::None
    );

    assert!(matches!(core.bottom.owner(), BottomOwner::Composer));
    assert_eq!(core.bottom.composer().submit_text(), "");
}

#[test]
fn shifted_slash_opens_palette_when_idle_composer_is_empty() {
    let mut core = core();

    assert_eq!(
        core.handle_input(modified_key(KeyCode::Char('/'), KeyModifiers::SHIFT)),
        CoreEffect::Render
    );

    assert!(matches!(core.bottom.owner(), BottomOwner::Palette(_)));
    assert_eq!(core.bottom.composer().submit_text(), "");
}

#[test]
fn control_slash_remains_literal_text_when_idle_composer_is_empty() {
    let mut core = core();

    assert_eq!(
        core.handle_input(modified_key(KeyCode::Char('/'), KeyModifiers::CONTROL)),
        CoreEffect::Render
    );

    assert!(matches!(core.bottom.owner(), BottomOwner::Composer));
    assert_eq!(core.bottom.composer().submit_text(), "/");
}

#[test]
fn slash_edits_next_draft_while_turn_is_in_flight_when_draft_is_not_empty() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    core.bottom.composer_mut().insert_text("path ");

    assert_eq!(
        core.handle_input(key(KeyCode::Char('/'))),
        CoreEffect::Render
    );

    assert!(matches!(core.bottom.owner(), BottomOwner::Composer));
    assert_eq!(core.bottom.composer().submit_text(), "path /");
}

#[test]
fn active_turn_bottom_surface_accepts_palette_input() {
    let mut core = core();
    core.handle_input(key(KeyCode::Char('/')));
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    assert_eq!(
        core.handle_input(key(KeyCode::Char('x'))),
        CoreEffect::Render
    );
    assert_eq!(
        core.handle_input(InputEvent::Paste("pasted".to_owned())),
        CoreEffect::None
    );

    let BottomOwner::Palette(palette) = core.bottom.owner() else {
        panic!("palette should still own input");
    };
    assert_eq!(palette.input(), "/x");
    assert!(core.bottom.composer().submit_text().is_empty());
}

#[test]
fn slash_palette_quit_works_while_turn_is_in_flight() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    assert_eq!(
        core.handle_input(key(KeyCode::Char('/'))),
        CoreEffect::Render
    );
    for ch in "quit".chars() {
        assert_eq!(
            core.handle_input(key(KeyCode::Char(ch))),
            CoreEffect::Render
        );
    }
    assert_eq!(core.handle_input(key(KeyCode::Enter)), CoreEffect::Quit);
}

#[test]
fn modal_input_keeps_precedence_while_turn_is_in_flight() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    core.modal = Some(Modal::Permission(fs_write_request()));

    assert_eq!(
        core.handle_input(key(KeyCode::Char('x'))),
        CoreEffect::Render
    );
    assert_eq!(
        core.handle_input(InputEvent::Paste("pasted".to_owned())),
        CoreEffect::Render
    );

    assert!(matches!(core.modal, Some(Modal::Permission(_))));
    assert_eq!(core.bottom.composer().submit_text(), "xpasted");
}

#[test]
fn active_turn_interrupt_still_wins_when_bottom_surface_owns_input() {
    let mut core = core();
    core.handle_input(key(KeyCode::Char('/')));
    let interrupt_flag = Arc::new(AtomicBool::new(false));
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::clone(&interrupt_flag),
        started_at: Instant::now(),
    };

    assert_eq!(
        core.handle_input(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        CoreEffect::Render
    );

    assert!(interrupt_flag.load(Ordering::SeqCst));
    assert!(core.interrupted_guidance);
}

#[test]
fn active_turn_escape_interrupts_when_bottom_surface_owns_input() {
    let mut core = core();
    core.handle_input(key(KeyCode::Char('/')));
    let interrupt_flag = Arc::new(AtomicBool::new(false));
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::clone(&interrupt_flag),
        started_at: Instant::now(),
    };

    assert_eq!(core.handle_input(key(KeyCode::Esc)), CoreEffect::Render);

    assert!(interrupt_flag.load(Ordering::SeqCst));
    assert!(core.interrupted_guidance);
}

#[test]
fn next_draft_survives_active_turn_completion_and_can_submit() {
    let mut core = core_with_provider(SlowEchoProvider);
    core.handle_input(key(KeyCode::Char('h')));
    assert_eq!(core.handle_input(key(KeyCode::Enter)), CoreEffect::Render);
    assert!(core.turn_in_flight());

    for ch in "next".chars() {
        core.handle_input(key(KeyCode::Char(ch)));
    }
    assert_eq!(core.bottom.composer().submit_text(), "next");

    for _ in 0..50 {
        if core.drain_background() && !core.turn_in_flight() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(!core.turn_in_flight());
    assert_eq!(core.bottom.composer().submit_text(), "next");
    assert_eq!(core.handle_input(key(KeyCode::Enter)), CoreEffect::Render);
    assert!(core.turn_in_flight());
    assert_eq!(core.bottom.composer().submit_text(), "");
}

#[test]
fn active_turn_frame_shows_working_state_and_next_draft() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    for ch in "next draft".chars() {
        core.handle_input(key(KeyCode::Char(ch)));
    }

    let frame = core.visual_canvas_frame(80);
    let text = frame
        .active_frame_lines()
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("⠋ working"), "frame: {text:?}");
    assert!(text.contains("▌ next draft"), "frame: {text:?}");
}

#[test]
fn active_turn_live_transcript_prefix_stays_after_commit_boundary() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_DELTA,
        object([
            ("kind", "text".into()),
            ("delta", "line one\nline two\nline three\n".into()),
        ]),
    )));

    let frame = core.visual_canvas_frame(80);
    let text = frame
        .active_frame_lines()
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("line one"), "frame: {text:?}");
    assert!(text.contains("⠋ working"), "frame: {text:?}");
    let first_live = frame
        .active_frame_lines()
        .iter()
        .position(|line| line.plain_text().contains("line one"))
        .expect("live line");
    assert!(first_live >= frame.committable_rows);
    assert!(frame.committable_rows < frame.active_frame_lines().len());
}

#[test]
fn active_turn_mutable_live_tail_is_not_committable() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_DELTA,
        object([
            ("kind", "text".into()),
            ("delta", "stable prefix\nmutable tail".into()),
        ]),
    )));

    let frame = core.visual_canvas_frame(80);
    let text = frame
        .active_frame_lines()
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>();
    let stable = text
        .iter()
        .position(|line| line.contains("stable prefix"))
        .expect("stable line should render");
    let mutable = text
        .iter()
        .position(|line| line.contains("mutable tail"))
        .expect("mutable line should render");

    assert!(stable >= frame.committable_rows, "frame: {text:?}");
    assert!(
        mutable >= frame.committable_rows,
        "mutable row crossed commit boundary: {text:?}"
    );
}

#[test]
fn active_turn_open_code_fence_is_visible_but_not_committable() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_DELTA,
        object([
            ("kind", "text".into()),
            ("delta", "stable prefix\n```rust\nlet x = 1;\n".into()),
        ]),
    )));

    let frame = core.visual_canvas_frame(80);
    let text = frame
        .active_frame_lines()
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>();
    let stable = text
        .iter()
        .position(|line| line.contains("stable prefix"))
        .expect("stable line should render");
    let code = text
        .iter()
        .position(|line| line.contains("let x = 1;"))
        .expect("open fence code should render as live mutable text");

    assert!(stable >= frame.committable_rows, "frame: {text:?}");
    assert!(
        code >= frame.committable_rows,
        "open code fence crossed commit boundary: {text:?}"
    );
}

#[test]
fn active_turn_bare_table_partial_row_stays_outside_commit_boundary() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_DELTA,
        object([
            ("kind", "text".into()),
            ("delta", "| A | B |\n|---|---|\n| partial | row |".into()),
        ]),
    )));

    let frame = core.visual_canvas_frame(80);
    let text = frame
        .active_frame_lines()
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains('A'), "frame: {text:?}");
    assert!(
        !text.contains("partial"),
        "partial row should be held: {text:?}"
    );
    assert!(frame.committable_rows > 0);
    assert!(frame.committable_rows < frame.active_frame_lines().len());
}

#[test]
fn ctrl_o_expands_and_refolds_finalized_shell_artifacts() {
    let mut core = core();
    core.push_finalized_visual_item(shell_artifact_with_lines(12));

    let folded = drain_finalized_visual_text(&mut core, 80);
    assert!(folded.contains("7 more lines"), "folded: {folded:?}");
    assert!(!folded.contains("line 3"), "folded: {folded:?}");

    assert_eq!(core.handle_input(key(KeyCode::PageUp)), CoreEffect::Render);
    assert_eq!(core.visual_scroll_offset(), 8);
    assert_eq!(
        core.handle_input(ctrl_o()),
        CoreEffect::ReplayHistoryWithScrollbackPurge
    );
    assert_eq!(core.visual_scroll_offset(), 0);
    let expanded = drain_finalized_visual_text(&mut core, 80);
    assert!(!expanded.contains("more lines"), "expanded: {expanded:?}");
    assert!(expanded.contains("line 3"), "expanded: {expanded:?}");
    assert_eq!(core.bottom.composer().submit_text(), "");

    assert_eq!(
        core.handle_input(ctrl_o()),
        CoreEffect::ReplayHistoryWithScrollbackPurge
    );
    let refolded = drain_finalized_visual_text(&mut core, 80);
    assert!(refolded.contains("7 more lines"), "refolded: {refolded:?}");
    assert!(!refolded.contains("line 3"), "refolded: {refolded:?}");
}

#[test]
fn ctrl_o_expands_and_refolds_finalized_reasoning_items() {
    // Review finding: the collapsed thought line advertises "ctrl+o expand"
    // but ModelReasoning was not classified foldable, so ctrl+o never
    // targeted it. Verify the toggle actually reaches a reasoning item.
    let mut core = core();
    core.push_finalized_visual_item(TranscriptItem::ModelReasoning {
        fidelity: "raw".to_owned(),
        content: "First sentence stays short.\nSecond paragraph reveals much \
                  more detail that only appears once ctrl+o expands the full thought."
            .to_owned(),
    });

    let folded = drain_finalized_visual_text(&mut core, 80);
    assert!(folded.contains("ctrl+o expand"), "folded: {folded:?}");
    assert!(!folded.contains("Second paragraph"), "folded: {folded:?}");

    assert_eq!(
        core.handle_input(ctrl_o()),
        CoreEffect::ReplayHistoryWithScrollbackPurge
    );
    let expanded = drain_finalized_visual_text(&mut core, 80);
    assert!(
        expanded.contains("ctrl+o collapse"),
        "expanded: {expanded:?}"
    );
    assert!(
        expanded.contains("Second paragraph"),
        "expanded: {expanded:?}"
    );

    assert_eq!(
        core.handle_input(ctrl_o()),
        CoreEffect::ReplayHistoryWithScrollbackPurge
    );
    let refolded = drain_finalized_visual_text(&mut core, 80);
    assert!(refolded.contains("ctrl+o expand"), "refolded: {refolded:?}");
    assert!(
        !refolded.contains("Second paragraph"),
        "refolded: {refolded:?}"
    );
}

#[test]
fn ctrl_o_expands_and_refolds_terminal_rendered_shell_artifacts() {
    let mut terminal =
        crate::ui::terminal::InlineTerminal::new(VT100Backend::new(80, 10), 10).expect("terminal");
    terminal.set_linefeed_history_insert_enabled(true);
    let mut core = core();
    core.push_finalized_visual_item(shell_artifact_with_lines(48));

    terminal
        .draw_visual_frame(&core.visual_canvas_frame(80))
        .expect("draw folded");
    let folded = terminal.backend().screen_contents();
    assert!(folded.contains("43 more lines"), "folded: {folded:?}");
    assert!(!folded.contains("line 3"), "folded: {folded:?}");
    let folded_scrollback = terminal.backend().scrollback_rows().join("\n");
    assert!(
        folded_scrollback.contains("43 more lines"),
        "folded summary should commit to native scrollback: {folded_scrollback:?}"
    );

    assert_eq!(
        core.handle_input(ctrl_o()),
        CoreEffect::ReplayHistoryWithScrollbackPurge
    );
    terminal.backend_mut().clear_raw_output();
    terminal
        .reset_for_history_replay(true)
        .expect("reset for expanded replay");
    let reset_raw = terminal.backend().raw_output();
    assert!(
        reset_raw
            .windows(b"\x1b[3J".len())
            .any(|window| window == b"\x1b[3J"),
        "replay must purge stale terminal scrollback: {reset_raw:?}"
    );
    terminal.backend_mut().clear_raw_output();
    terminal
        .draw_visual_frame(&core.visual_canvas_frame(80))
        .expect("draw expanded");
    let expanded = terminal.backend().screen_contents();
    assert!(!expanded.contains("more lines"), "expanded: {expanded:?}");
    assert!(expanded.contains("line 48"), "expanded: {expanded:?}");
    let expanded_scrollback = terminal.backend().scrollback_rows().join("\n");
    assert!(
        expanded_scrollback.contains("line 48"),
        "expanded replay should commit visible source rows: {expanded_scrollback:?}"
    );

    assert_eq!(
        core.handle_input(ctrl_o()),
        CoreEffect::ReplayHistoryWithScrollbackPurge
    );
    terminal.backend_mut().clear_raw_output();
    terminal
        .reset_for_history_replay(true)
        .expect("reset for refold replay");
    let reset_raw = terminal.backend().raw_output();
    assert!(
        reset_raw
            .windows(b"\x1b[3J".len())
            .any(|window| window == b"\x1b[3J"),
        "refold replay must purge stale terminal scrollback: {reset_raw:?}"
    );
    terminal.backend_mut().clear_raw_output();
    terminal
        .draw_visual_frame(&core.visual_canvas_frame(80))
        .expect("draw refolded");
    let refolded = terminal.backend().screen_contents();
    assert!(refolded.contains("43 more lines"), "refolded: {refolded:?}");
    assert!(!refolded.contains("line 3"), "refolded: {refolded:?}");
    let refolded_scrollback = terminal.backend().scrollback_rows().join("\n");
    assert!(
        refolded_scrollback.contains("43 more lines"),
        "refold replay should commit folded summary: {refolded_scrollback:?}"
    );
}

#[test]
fn history_replay_clear_uses_theme_background() {
    let mut terminal =
        crate::ui::terminal::InlineTerminal::new(VT100Backend::new(30, 4), 3).expect("terminal");
    terminal
        .set_theme_colors(
            Color::Rgb(60, 56, 54),
            Color::Rgb(251, 241, 199),
            Color::Rgb(60, 56, 54),
            Color::Rgb(142, 192, 124),
        )
        .expect("theme colors");
    if std::env::var_os("NO_COLOR").is_some() {
        return;
    }

    terminal.backend_mut().clear_raw_output();
    terminal
        .reset_for_history_replay(true)
        .expect("reset for history replay");
    let reset_raw = terminal.backend().raw_output();

    assert!(
        reset_raw
            .windows(b"\x1b[48;2;251;241;199m".len())
            .any(|window| window == b"\x1b[48;2;251;241;199m"),
        "replay reset must clear with theme background: {reset_raw:?}"
    );
}

#[test]
fn live_file_diff_replaces_prior_patch_preview_for_same_path() {
    let mut core = core();
    core.push_finalized_visual_item(TranscriptItem::PatchApplied {
        path: "src/lib.rs".to_owned(),
        old: None,
        new: Some("alpha_patch_history\n".to_owned()),
    });
    core.push_finalized_visual_item(TranscriptItem::ModelResult("between edits".to_owned()));
    core.push_finalized_visual_item(TranscriptItem::PatchApplied {
        path: "src/lib.rs".to_owned(),
        old: None,
        new: Some("beta_stale_preview\n".to_owned()),
    });
    core.push_finalized_visual_item(TranscriptItem::FileChange {
        path: "src/lib.rs".to_owned(),
        action: "add".to_owned(),
        origin: "apply_patch".to_owned(),
        before_sha256: None,
        after_sha256: Some("abc123".to_owned()),
        before_byte_len: None,
        after_byte_len: Some(20),
        diff_redaction: "omitted".to_owned(),
        checkpoint_event_id: None,
    });
    core.push_finalized_visual_item(TranscriptItem::FileDiff {
        path: "src/lib.rs".to_owned(),
        action: "add".to_owned(),
        origin: "apply_patch".to_owned(),
        diff: Some("@@ -0,0 +1 @@\n+canonical_file_diff\n".to_owned()),
        truncated: false,
        truncation: String::new(),
        omitted_reason: None,
        checkpoint_event_id: None,
    });

    let rendered = core
        .visual_canvas_frame(96)
        .active_frame_lines()
        .iter()
        .map(|line| line.plain_text())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        rendered.contains("alpha_patch_history"),
        "rendered: {rendered:?}"
    );
    assert!(rendered.contains("between edits"), "rendered: {rendered:?}");
    assert!(
        !rendered.contains("beta_stale_preview"),
        "rendered: {rendered:?}"
    );
    assert!(
        !rendered.contains("File added src/lib.rs"),
        "rendered: {rendered:?}"
    );
    assert!(
        rendered.contains("canonical_file_diff"),
        "rendered: {rendered:?}"
    );
}

#[test]
fn idle_foldable_shell_artifacts_can_commit_with_replayable_ctrl_o() {
    let mut core = core();
    core.push_finalized_visual_item(shell_artifact_with_lines(48));
    core.push_finalized_visual_item(TranscriptItem::ModelResult("after foldable".to_owned()));

    let folded = core.visual_canvas_frame(80);
    assert!(folded.committable_rows > 0);

    assert_eq!(
        core.handle_input(ctrl_o()),
        CoreEffect::ReplayHistoryWithScrollbackPurge
    );
    let expanded = core.visual_canvas_frame(80);
    assert!(expanded.committable_rows > 0);
}

#[test]
fn short_shell_artifacts_allow_following_history_to_commit() {
    let mut core = core();
    core.push_finalized_visual_item(shell_artifact_with_lines(4));
    core.push_finalized_visual_item(TranscriptItem::ModelResult("after short output".to_owned()));

    let frame = core.visual_canvas_frame(80);
    assert!(frame.committable_rows > 0);
}

#[test]
fn active_turn_finalized_shell_artifacts_are_committable_without_live_markdown() {
    let mut core = core();
    core.push_finalized_visual_item(shell_artifact_with_lines(12));
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    let frame = core.visual_canvas_frame(80);

    assert!(frame.committable_rows > 0);
    assert!(frame.committable_rows <= frame.history_rows);
}

#[test]
fn active_turn_finalized_shell_artifacts_do_not_commit_mutable_live_text() {
    let mut core = core();
    core.push_finalized_visual_item(shell_artifact_with_lines(12));
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_DELTA,
        object([
            ("kind", "text".into()),
            ("delta", "mutable assistant tail".into()),
        ]),
    )));

    let frame = core.visual_canvas_frame(80);
    let text = frame
        .active_frame_lines()
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>();
    let mutable = text
        .iter()
        .position(|line| line.contains("mutable assistant tail"))
        .expect("mutable live text should render");

    assert!(frame.committable_rows > 0);
    assert!(frame.committable_rows <= frame.history_rows);
    assert!(
        mutable >= frame.committable_rows,
        "mutable live text crossed commit boundary: {text:?}"
    );
}

/// Issue #49: `ctrl+o` is a single global toggle — every foldable cell in a
/// mixed transcript (tool run, reasoning, diff) expands together on the
/// first press and collapses together on the second, with no per-cell
/// targeting or invisible nearest-to-viewport heuristic.
#[test]
fn ctrl_o_expands_all_foldable_artifacts_globally() {
    let mut core = core();
    core.push_finalized_visual_item(shell_artifact_with_lines(12));
    core.push_finalized_visual_item(TranscriptItem::ModelReasoning {
        fidelity: "detailed".to_owned(),
        content: "Weighing the tradeoffs between approach A and approach B. \
            After a long deliberation the conclusion lands on approach B \
            because it is the option that fully honors the unique reasoning marker."
            .to_owned(),
    });
    core.push_finalized_visual_item(TranscriptItem::PatchApplied {
        path: "src/lib.rs".to_owned(),
        old: None,
        new: Some(
            (1..=20)
                .map(|line| format!("added line {line}"))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        ),
    });
    core.push_finalized_visual_item(shell_artifact_with_lines(14));

    let folded = drain_finalized_visual_text(&mut core, 80);
    assert!(folded.contains("7 more lines"), "folded: {folded:?}");
    assert!(folded.contains("9 more lines"), "folded: {folded:?}");
    assert!(folded.contains("ctrl+o expand"), "folded: {folded:?}");
    assert!(
        !folded.contains("unique reasoning marker"),
        "folded: {folded:?}"
    );
    assert!(!folded.contains("added line 15"), "folded: {folded:?}");

    assert_eq!(
        core.handle_input(ctrl_o()),
        CoreEffect::ReplayHistoryWithScrollbackPurge
    );
    let expanded = drain_finalized_visual_text(&mut core, 80);

    // All foldable cells expand together — none remain collapsed.
    assert!(!expanded.contains("more lines"), "expanded: {expanded:?}");
    assert!(expanded.contains("line 3"), "expanded: {expanded:?}");
    assert!(
        expanded.contains("unique reasoning marker"),
        "expanded: {expanded:?}"
    );
    assert!(
        expanded.contains("ctrl+o collapse"),
        "expanded: {expanded:?}"
    );
    assert!(expanded.contains("added line 15"), "expanded: {expanded:?}");

    assert_eq!(
        core.handle_input(ctrl_o()),
        CoreEffect::ReplayHistoryWithScrollbackPurge
    );
    let refolded = drain_finalized_visual_text(&mut core, 80);
    assert!(refolded.contains("7 more lines"), "refolded: {refolded:?}");
    assert!(refolded.contains("9 more lines"), "refolded: {refolded:?}");
    assert!(
        !refolded.contains("unique reasoning marker"),
        "refolded: {refolded:?}"
    );
    assert!(
        !refolded.contains("added line 15"),
        "refolded: {refolded:?}"
    );
}

#[test]
fn ctrl_o_without_foldable_artifact_is_noop_and_does_not_edit_composer() {
    let mut core = core();
    core.push_finalized_visual_item(TranscriptItem::AssistantMessage("plain answer".to_owned()));

    assert_eq!(core.handle_input(ctrl_o()), CoreEffect::None);

    assert_eq!(core.bottom.composer().submit_text(), "");
    assert!(!drain_finalized_visual_text(&mut core, 80).contains("more lines"));
}

#[test]
fn footer_ctrl_o_hint_only_appears_when_a_foldable_artifact_exists() {
    let mut core = core();
    core.push_finalized_visual_item(TranscriptItem::AssistantMessage("plain answer".to_owned()));

    let without_fold = core.canvas_status_snapshot(120).line.plain_text();
    assert!(without_fold.contains("/ commands"));
    assert!(!without_fold.contains("ctrl+o expand"));

    core.push_finalized_visual_item(shell_artifact_with_lines(12));

    let with_fold = core.canvas_status_snapshot(120).line.plain_text();
    assert!(with_fold.contains("/ commands · ctrl+o expand"));
}

#[test]
fn ctrl_o_does_not_bypass_modal_or_palette_ownership() {
    let mut modal_core = core();
    modal_core.push_finalized_visual_item(shell_artifact_with_lines(12));
    modal_core.modal = Some(Modal::Permission(fs_write_request()));

    assert_eq!(modal_core.handle_input(ctrl_o()), CoreEffect::None);
    let modal_text = drain_finalized_visual_text(&mut modal_core, 80);
    assert!(
        modal_text.contains("7 more lines"),
        "modal_text: {modal_text:?}"
    );
    assert!(matches!(modal_core.modal, Some(Modal::Permission(_))));

    let mut palette_core = core();
    palette_core.push_finalized_visual_item(shell_artifact_with_lines(12));
    palette_core.handle_input(key(KeyCode::Char('/')));

    assert_eq!(palette_core.handle_input(ctrl_o()), CoreEffect::None);
    let palette_text = drain_finalized_visual_text(&mut palette_core, 80);
    assert!(
        palette_text.contains("7 more lines"),
        "palette_text: {palette_text:?}"
    );
    let BottomOwner::Palette(palette) = palette_core.bottom.owner() else {
        panic!("palette should still own input");
    };
    assert_eq!(palette.input(), "/");
}

#[test]
fn ctrl_o_can_expand_previous_artifacts_while_turn_is_in_flight() {
    let mut core = core();
    core.push_finalized_visual_item(shell_artifact_with_lines(12));
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    assert_eq!(
        core.handle_input(ctrl_o()),
        CoreEffect::ReplayHistoryWithScrollbackPurge
    );

    let frame = core.visual_canvas_frame(80);
    assert!(frame.committable_rows > 0);
    assert!(frame.committable_rows <= frame.history_rows);
    let text = frame
        .active_frame_lines()
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains("more lines"), "text: {text:?}");
    assert!(text.contains("line 3"), "text: {text:?}");
    assert!(core.turn_in_flight());

    assert_eq!(
        core.handle_input(ctrl_o()),
        CoreEffect::ReplayHistoryWithScrollbackPurge
    );
    let refolded = core.visual_canvas_frame(80);
    let refolded_text = refolded
        .active_frame_lines()
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(refolded.committable_rows > 0);
    assert!(refolded.committable_rows <= refolded.history_rows);
    assert!(
        refolded_text.contains("more lines"),
        "refolded_text: {refolded_text:?}"
    );
    assert!(core.turn_in_flight());
}

#[test]
fn replay_history_effect_wins_over_render_but_not_quit() {
    assert_eq!(
        merge_effects(
            CoreEffect::ReplayHistoryWithScrollbackPurge,
            CoreEffect::ReplayHistory
        ),
        CoreEffect::ReplayHistoryWithScrollbackPurge
    );
    assert_eq!(
        merge_effects(
            CoreEffect::Render,
            CoreEffect::ReplayHistoryWithScrollbackPurge
        ),
        CoreEffect::ReplayHistoryWithScrollbackPurge
    );
    assert_eq!(
        merge_effects(CoreEffect::ReplayHistory, CoreEffect::Render),
        CoreEffect::ReplayHistory
    );
    assert_eq!(
        merge_effects(CoreEffect::Render, CoreEffect::ReplayHistory),
        CoreEffect::ReplayHistory
    );
    assert_eq!(
        merge_effects(CoreEffect::ReplayHistory, CoreEffect::None),
        CoreEffect::ReplayHistory
    );
    assert_eq!(
        merge_effects(CoreEffect::TerminalClipboard, CoreEffect::Render),
        CoreEffect::TerminalClipboard
    );
    assert_eq!(
        merge_effects(CoreEffect::ThemeChanged, CoreEffect::Render),
        CoreEffect::ThemeChanged
    );
    assert_eq!(
        merge_effects(CoreEffect::TerminalClipboard, CoreEffect::ThemeChanged),
        CoreEffect::TerminalClipboard
    );
    assert_eq!(
        merge_effects(CoreEffect::ReplayHistory, CoreEffect::TerminalClipboard),
        CoreEffect::ReplayHistory
    );
    assert_eq!(
        merge_effects(
            CoreEffect::ReplayHistoryWithScrollbackPurge,
            CoreEffect::TerminalClipboard
        ),
        CoreEffect::ReplayHistoryWithScrollbackPurge
    );
    assert_eq!(
        merge_effects(
            CoreEffect::Quit,
            CoreEffect::ReplayHistoryWithScrollbackPurge
        ),
        CoreEffect::Quit
    );
    assert_eq!(
        merge_effects(CoreEffect::Quit, CoreEffect::TerminalClipboard),
        CoreEffect::Quit
    );
}

#[test]
fn external_editor_success_replaces_draft() {
    let mut core = core();
    core.handle_input(key(KeyCode::Char('o')));
    core.handle_input(key(KeyCode::Char('l')));
    core.handle_input(key(KeyCode::Char('d')));
    core.editor = Box::new(MockEditor {
        result: EditorResult::Updated("new\ntext".to_owned()),
    });

    assert_eq!(
        core.handle_input(modified_key(KeyCode::Char('x'), KeyModifiers::CONTROL)),
        CoreEffect::Render
    );

    assert_eq!(core.bottom.composer().submit_text(), "new\ntext");
    assert_eq!(core.notice.as_deref(), Some("draft updated from editor"));
}

#[test]
fn external_editor_receives_current_composer_draft() {
    let mut core = core();
    let drafts = Arc::new(Mutex::new(Vec::new()));
    core.handle_input(key(KeyCode::Char('d')));
    core.handle_input(key(KeyCode::Char('r')));
    core.handle_input(key(KeyCode::Char('a')));
    core.handle_input(key(KeyCode::Char('f')));
    core.handle_input(key(KeyCode::Char('t')));
    core.handle_input(modified_key(KeyCode::Enter, KeyModifiers::SHIFT));
    core.handle_input(key(KeyCode::Char('x')));
    core.editor = Box::new(CapturingEditor {
        drafts: Arc::clone(&drafts),
        result: EditorResult::Updated("edited".to_owned()),
    });

    core.handle_input(modified_key(KeyCode::Char('x'), KeyModifiers::CONTROL));

    assert_eq!(
        *drafts.lock().expect("editor drafts lock"),
        vec!["draft\nx"]
    );
    assert_eq!(core.bottom.composer().submit_text(), "edited");
}

#[test]
fn external_editor_unset_preserves_draft_and_shows_notice() {
    let mut core = core();
    core.handle_input(key(KeyCode::Char('k')));
    core.handle_input(key(KeyCode::Char('e')));
    core.handle_input(key(KeyCode::Char('p')));
    core.editor = Box::new(MockEditor {
        result: EditorResult::Unset,
    });

    core.handle_input(modified_key(KeyCode::Char('x'), KeyModifiers::CONTROL));

    assert_eq!(core.bottom.composer().submit_text(), "kep");
    assert_eq!(
        core.notice.as_deref(),
        Some("EDITOR is not set; draft unchanged")
    );
}

#[test]
fn external_editor_failure_preserves_draft_and_shows_notice() {
    let mut core = core();
    core.handle_input(key(KeyCode::Char('s')));
    core.handle_input(key(KeyCode::Char('a')));
    core.handle_input(key(KeyCode::Char('f')));
    core.handle_input(key(KeyCode::Char('e')));
    core.editor = Box::new(MockEditor {
        result: EditorResult::Failed("editor exited with a non-zero status".to_owned()),
    });

    core.handle_input(modified_key(KeyCode::Char('x'), KeyModifiers::CONTROL));

    assert_eq!(core.bottom.composer().submit_text(), "safe");
    assert_eq!(
        core.notice.as_deref(),
        Some("editor failed: editor exited with a non-zero status; draft unchanged")
    );
}

#[test]
fn name_session_reports_metadata_refresh_failure_after_durable_rename() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = EulerHome::from_root(temp.path().join(".euler")).expect("home");
    let store = SessionStore::new(home).expect("store");
    let record = store.create_session().expect("session record");
    let (decider, channels) = TuiDecider::new();
    let mut config = euler_core::SessionConfig::new(temp.path());
    config.session_id = record.id().to_owned();
    config.agent_id = "tui-test".to_owned();
    config.model = "echo".to_owned();
    let session = Session::new(config, EchoProvider, decider)
        .with_provenance(ProvenanceWriter::new(record.events_path()).expect("writer"));
    let mut core = AppCore::new(session, channels);
    let alternate_home = EulerHome::from_root(temp.path().join(".other-euler")).expect("home");
    core.session_store = Some(SessionStore::new(alternate_home).expect("alternate store"));

    assert_eq!(
        core.name_current_session("  honest   name  ".to_owned()),
        CoreEffect::Render
    );

    // Routed through the shared spine notice (review v4 dogfood), not the
    // transient `self.notice` banner: same treatment as `theme set to …`,
    // anchored on the spine rather than rendering flush at column 0.
    let notice = drain_finalized_visual_text(&mut core, 80);
    // Pin the `•` spine anchor (review v2 §14.4): the bug rendered this
    // notice flush at column 0 with no bullet, unlike every other setting
    // confirmation.
    assert!(
        notice.contains("\u{2022} session named honest name; metadata refresh failed:"),
        "notice: {notice:?}"
    );
    assert!(notice.contains("session not found"));
    assert!(!notice.contains("session naming failed:"));
    let events = read_resume_prefix(record.events_path()).expect("events");
    let rename = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::SESSION_RENAMED)
        .expect("rename event");
    assert_eq!(
        rename
            .payload
            .get("name")
            .and_then(serde_json::Value::as_str),
        Some("honest name")
    );
}

#[test]
fn name_session_refreshes_metadata_after_durable_rename() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = EulerHome::from_root(temp.path().join(".euler")).expect("home");
    let store = SessionStore::new(home).expect("store");
    let record = store.create_session().expect("session record");
    let (decider, channels) = TuiDecider::new();
    let mut config = euler_core::SessionConfig::new(temp.path());
    config.session_id = record.id().to_owned();
    config.agent_id = "tui-test".to_owned();
    config.model = "echo".to_owned();
    let session = Session::new(config, EchoProvider, decider)
        .with_provenance(ProvenanceWriter::new(record.events_path()).expect("writer"));
    let mut core = AppCore::new(session, channels);
    core.session_store = Some(store.clone());

    assert_eq!(
        core.name_current_session("  clean   name  ".to_owned()),
        CoreEffect::Render
    );

    // Routed through the shared spine notice (review v4 dogfood): same
    // treatment as `theme set to …`, not the transient `self.notice` banner.
    // Pin the `•` spine anchor — the bug rendered this flush at column 0.
    let notice = drain_finalized_visual_text(&mut core, 80);
    assert!(
        notice.contains("\u{2022} session named clean name"),
        "notice: {notice:?}"
    );
    let refreshed = store
        .find_session(record.id())
        .expect("find session")
        .expect("session record");
    assert_eq!(refreshed.name(), Some("clean name"));

    // #46: the footer picks up the name from the same render, no extra
    // rebuild needed — and even though naming failed the metadata refresh
    // in the sibling test above, the footer there still updates (asserted
    // separately below) because it never depended on that refresh.
    assert_eq!(core.status.session_name.as_deref(), Some("clean name"));
    let rendered = core.canvas_status_snapshot(120).line.plain_text();
    assert!(rendered.ends_with("echo(medium) · ctx ?% · clean name"));
}

#[test]
fn name_session_updates_footer_immediately_even_if_metadata_refresh_fails() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = EulerHome::from_root(temp.path().join(".euler")).expect("home");
    let store = SessionStore::new(home).expect("store");
    let record = store.create_session().expect("session record");
    let (decider, channels) = TuiDecider::new();
    let mut config = euler_core::SessionConfig::new(temp.path());
    config.session_id = record.id().to_owned();
    config.agent_id = "tui-test".to_owned();
    config.model = "echo".to_owned();
    let session = Session::new(config, EchoProvider, decider)
        .with_provenance(ProvenanceWriter::new(record.events_path()).expect("writer"));
    let mut core = AppCore::new(session, channels);
    let alternate_home = EulerHome::from_root(temp.path().join(".other-euler")).expect("home");
    core.session_store = Some(SessionStore::new(alternate_home).expect("alternate store"));

    assert_eq!(core.status.session_name, None);
    assert_eq!(
        core.name_current_session("still named".to_owned()),
        CoreEffect::Render
    );

    assert_eq!(core.status.session_name.as_deref(), Some("still named"));
    let rendered = core.canvas_status_snapshot(120).line.plain_text();
    assert!(rendered.ends_with("echo(medium) · ctx ?% · still named"));
}

#[test]
fn reasoning_effort_action_updates_status_session_and_events() {
    let mut core = core();

    assert_eq!(
        core.set_reasoning_effort(ReasoningEffort::XLarge),
        CoreEffect::Render
    );

    assert_eq!(core.status.reasoning_effort.as_deref(), Some("xlarge"));
    assert!(core
        .canvas_status_snapshot(120)
        .line
        .plain_text()
        .ends_with("echo(xlarge) · ctx ?%"));
    let AppState::Idle { session } = &core.state else {
        panic!("session should be idle");
    };
    assert_eq!(session.reasoning_effort(), ReasoningEffort::XLarge);
    assert!(session.events().iter().any(|event| {
        event.kind.as_str() == EventKind::MODEL_EFFORT_CHANGED
            && event
                .payload
                .get("to_effort")
                .and_then(serde_json::Value::as_str)
                == Some("xlarge")
    }));
}

#[test]
fn new_session_reuses_target_and_purges_visual_history() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = EulerHome::from_root(temp.path().join(".euler")).expect("home");
    let store = SessionStore::new(home).expect("store");
    let mut core = core_with_fixture_catalog(
        EchoProvider,
        "echo",
        fixture_catalog_with_windows(&[("echo", 1_000)]),
    );
    core.session_store = Some(store.clone());
    core.primary_agent_id = Some("stale-owner".to_owned());
    core.transcript.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "old content".into())]),
    ));

    assert_eq!(
        core.start_new_session(),
        CoreEffect::ReplayHistoryWithScrollbackPurge
    );

    assert!(core
        .notice
        .as_deref()
        .is_some_and(|notice| notice.starts_with("new session ")));
    assert!(!drain_finalized_visual_text(&mut core, 80).contains("old content"));
    let AppState::Idle { session } = &core.state else {
        panic!("session should be idle");
    };
    assert_eq!(session.active_target().model, "echo");
    assert!(store
        .find_session(session.session_id())
        .expect("find")
        .is_some());
    assert_eq!(
        core.status.session_id.as_deref(),
        Some(session.session_id())
    );
    assert_eq!(core.token_usage.session_cost_picos, 0);
    assert_eq!(core.token_usage.priced_calls, 0);
    assert_eq!(core.token_usage.unpriced_calls, 0);
    assert_eq!(core.token_usage.input_tokens, 0);
    assert_eq!(core.token_usage.context_window_tokens, Some(1_000));
    assert_eq!(core.primary_agent_id.as_deref(), Some("root"));
    assert!(core
        .canvas_status_snapshot(120)
        .line
        .plain_text()
        .ends_with("echo(medium) · ctx 0%"));
}

#[test]
fn export_session_writes_current_events_json() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("session.json");
    let mut core = core();

    assert_eq!(
        core.export_session(Some(out.display().to_string())),
        CoreEffect::Render
    );

    let exported: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out).expect("read export"))
            .expect("export json");
    assert_eq!(exported["model"], "echo");
    assert_eq!(exported["reasoning_effort"], "medium");
    assert!(exported["events"]
        .as_array()
        .is_some_and(|events| !events.is_empty()));
}

#[test]
fn export_session_without_path_writes_under_private_home_exports() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = EulerHome::from_root(temp.path().join(".euler")).expect("home");
    let store = SessionStore::new(home).expect("store");
    let mut core = core();
    core.session_store = Some(store.clone());
    let AppState::Idle { session } = &core.state else {
        panic!("session should be idle");
    };
    let expected = store
        .home()
        .root()
        .join("exports")
        .join(format!("euler-session-{}.json", session.session_id()));

    assert_eq!(core.export_session(None), CoreEffect::Render);

    assert!(expected.is_file());
    assert!(drain_finalized_visual_text(&mut core, 80).contains("session exported to"));
    let exported: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&expected).expect("read export"))
            .expect("export json");
    assert_eq!(exported["model"], "echo");
}

#[cfg(unix)]
#[test]
fn export_session_creates_private_files() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("nested").join("session.json");
    let mut core = core();

    assert_eq!(
        core.export_session(Some(out.display().to_string())),
        CoreEffect::Render
    );

    let mode = std::fs::metadata(out)
        .expect("export metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn export_session_omits_runtime_only_model_delta_and_stays_parent_closed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("session.json");
    let mut core = core();

    // Drive a real turn through EchoProvider, which streams a TextDelta
    // before finishing; this produces at least one model.delta event in
    // session.events() (runtime-only, never persisted per
    // docs/contracts/events.md).
    for ch in "hello".chars() {
        core.handle_input(key(KeyCode::Char(ch)));
    }
    core.handle_input(key(KeyCode::Enter));
    wait_for_idle(&mut core);

    let AppState::Idle { session } = &core.state else {
        panic!("session should be idle");
    };
    assert!(
        session
            .events()
            .iter()
            .any(|event| event.kind.as_str() == EventKind::MODEL_DELTA),
        "test setup should produce a model.delta event to filter"
    );

    assert_eq!(
        core.export_session(Some(out.display().to_string())),
        CoreEffect::Render
    );

    let exported: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out).expect("read export"))
            .expect("export json");
    let events = exported["events"].as_array().expect("events array").clone();
    assert!(!events.is_empty());

    let exported_ids: std::collections::BTreeSet<&str> = events
        .iter()
        .map(|event| event["id"].as_str().expect("event id"))
        .collect();

    for event in &events {
        assert_ne!(
            event["kind"].as_str(),
            Some(EventKind::MODEL_DELTA),
            "exported events must never include runtime-only model.delta"
        );
        if let Some(parent) = event["parent"].as_str() {
            assert!(
                exported_ids.contains(parent),
                "exported event {:?} parents {parent:?}, which is missing from the export",
                event["id"]
            );
        }
    }
}

#[test]
fn status_reports_session_id_while_turn_is_in_flight() {
    let mut core = core();
    let session_id = core.status.session_id.clone().expect("session id");
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    assert_eq!(core.show_status(), CoreEffect::Render);

    let text = drain_finalized_visual_text(&mut core, 80);
    assert!(text.contains(&format!("session: {session_id}")));
    assert!(text.contains("theme: Gruvbox Dark (gruvbox-dark)"));
    assert!(!text.contains("session: none"));
    // No permissions line is asserted here: this test enters TurnInFlight by
    // hand, so no handoff ever snapshotted an envelope. Production can only
    // reach that state through `spawn_*`, which does. The envelope's mid-turn
    // behavior is covered by the `permission_envelope_*` tests, which hand a
    // real session off.
}

/// The most recent `permissions:` line from the rendered transcript. The
/// transcript accumulates, so asserting `!contains(...)` over the whole thing
/// matches earlier notices ("permission posture set to Full access") rather
/// than the status line under test.
fn last_permissions_line(core: &mut AppCore) -> String {
    assert_eq!(core.show_status(), CoreEffect::Render);
    let text = drain_finalized_visual_text(core, 80);
    text.lines()
        .rev()
        .find(|line| line.contains("permissions:"))
        .unwrap_or_else(|| panic!("no permissions line in:\n{text}"))
        .to_owned()
}

/// §5.1: approving an uncovered capability for the session flips its mode to
/// SessionAllow (`PermissionGate::install_grant`), so a posture of "Ask every
/// time" stops being true *during* the turn — nobody in the UI chose it. The
/// cached envelope has to follow the session home, or /status keeps reporting
/// the pre-turn boundary.
///
/// The mode move is what an unscoped session approval does; core proves that
/// in `unscoped_session_grant_moves_the_mode_and_revoking_restores_it`. Here
/// the concern is only that the reported envelope tracks it.
#[test]
fn permission_envelope_follows_a_session_approval_across_the_turn_boundary() {
    let mut core = core();
    // Start from a posture that is actually in force — a fresh session's
    // capabilities are unset and match none, so it reads `custom` already.
    assert_eq!(
        core.set_permission_posture(PermissionPosture::AskEveryTime),
        CoreEffect::Render
    );
    let before = last_permissions_line(&mut core);
    assert!(before.contains("Ask every time"), "before: {before:?}");

    let mut session = core.take_idle_session();
    // What the worker's session looks like after the user answers `a`.
    session.set_permission_mode(Capability::ShellExec, ApprovalMode::SessionAllow);

    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    core.handle_turn_event(TurnEvent::TurnDone {
        outcome: TurnOutcome::Complete,
        session,
    });

    // Home again: derived live, so the approval is reflected without anything
    // having had to remember to refresh.
    let home = last_permissions_line(&mut core);
    assert!(
        home.contains("custom") && !home.contains("Ask every time"),
        "the approval changed the boundary; before was {before:?}, now {home:?}"
    );

    // And into the next turn, via the handoff snapshot.
    let session = core.take_idle_session();
    core.spawn_turn("next".to_owned(), session);
    let in_flight = last_permissions_line(&mut core);
    assert!(
        in_flight.contains("custom") && !in_flight.contains("Ask every time"),
        "stale mid-turn: {in_flight:?}"
    );
}

/// The cache answers /status for exactly one window — a turn in flight — and
/// the only way into that window is handing the session to a worker. So the
/// envelope is snapshotted at that boundary, which is what makes every idle
/// change reach it: a posture, a mode, a revoked grant, or a wholly different
/// session from `/new` or `/resume`. Refreshing at the sites that *change*
/// modes only ever covers the ones someone remembered.
#[test]
fn permission_envelope_is_snapshotted_when_the_session_is_handed_off() {
    let mut core = core();
    assert_eq!(
        core.set_permission_posture(PermissionPosture::AskEveryTime),
        CoreEffect::Render
    );
    assert_eq!(
        core.set_permission_mode(Capability::ShellExec, ApprovalMode::SessionAllow),
        CoreEffect::Render
    );
    // Nothing refreshed a cache on the way through: idle derives live.
    assert_eq!(core.status.permission_envelope, None);

    let session = core.take_idle_session();
    core.spawn_turn("first".to_owned(), session);

    let in_flight = last_permissions_line(&mut core);
    assert!(in_flight.contains("custom"), "line: {in_flight:?}");
}

/// The case no mutation-site refresh can cover: the session is *replaced*
/// (`/new`, `/resume`) rather than mutated, so nothing that changes modes ever
/// runs. The handoff still snapshots, because it snapshots whatever session it
/// is actually given.
#[test]
fn permission_envelope_follows_a_replaced_session_into_the_next_turn() {
    // Built first: `core` shadows the constructor below.
    let mut replacement = core();
    assert_eq!(
        replacement.set_permission_posture(PermissionPosture::ReadOnly),
        CoreEffect::Render
    );

    let mut core = core();
    assert_eq!(
        core.set_permission_posture(PermissionPosture::FullAccess),
        CoreEffect::Render
    );
    let first = core.take_idle_session();
    core.spawn_turn("first".to_owned(), first);
    assert!(
        core.status
            .permission_envelope
            .as_deref()
            .expect("snapshot")
            .starts_with("Full access"),
        "envelope: {:?}",
        core.status.permission_envelope
    );

    // A different session takes its place, with a different boundary — as
    // `/new` or `/resume` installs one. Nothing that mutates modes runs on
    // this path at all.
    let session = replacement.take_idle_session();
    core.state = AppState::Idle { session };

    let session = core.take_idle_session();
    core.spawn_turn("second".to_owned(), session);

    let in_flight = last_permissions_line(&mut core);
    assert!(
        in_flight.contains("Read only") && !in_flight.contains("Full access"),
        "stale envelope from the replaced session: {in_flight:?}"
    );
}

/// §5.1: the envelope states what the gate *effectively* does. Under Ask, a
/// statically-safe shell command (#78) and an operation already covered by a
/// durable grant both run with no prompt — so "every capability asks" is a
/// comfortable lie in the one line whose whole job is to be exact.
#[test]
fn permission_envelope_does_not_overstate_the_ask_boundary() {
    use crate::ui::commands::PermissionPosture;

    let ask = PermissionPosture::AskEveryTime.envelope();
    assert!(
        !ask.contains("every capability asks"),
        "envelope overstates the boundary: {ask:?}"
    );
    assert!(ask.contains("uncovered"), "envelope: {ask:?}");
    assert!(
        ask.contains("grants") && ask.contains("statically-safe"),
        "envelope must name what runs without a prompt: {ask:?}"
    );

    // Read only denies outright, so nothing is carved out of it: grants are
    // consulted under Ask, never under AlwaysDeny.
    let read_only = PermissionPosture::ReadOnly.envelope();
    assert!(read_only.contains("denied"), "envelope: {read_only:?}");
    assert!(
        !read_only.contains("ask"),
        "Read only never prompts: {read_only:?}"
    );

    // Every envelope states the boundary, not just the posture's name.
    for posture in PermissionPosture::ALL {
        let envelope = posture.envelope();
        assert!(
            envelope.contains(" · "),
            "envelope must state its boundary, not just a name: {envelope:?}"
        );
    }
}

#[test]
fn theme_action_switches_profile_and_context() {
    let mut core = core();

    assert_eq!(
        core.set_theme(ThemeChoice::GruvboxLight),
        CoreEffect::ThemeChanged
    );

    assert_eq!(core.theme_choice, ThemeChoice::GruvboxLight);
    assert_ne!(
        core.theme.palette.foreground,
        Theme::default_dark().palette.foreground
    );
    assert!(drain_finalized_visual_text(&mut core, 80).contains("theme set to gruvbox-light"));
}

#[test]
fn theme_options_seed_initial_profile() {
    let core = core_with_provider_model_options_at(
        EchoProvider,
        "echo",
        ".",
        AppOptions {
            theme_choice: ThemeChoice::GruvboxLight,
            ..AppOptions::default()
        },
    );

    assert_eq!(core.theme_choice, ThemeChoice::GruvboxLight);
    assert_eq!(
        core.theme.palette.background,
        Theme::default_light().palette.background
    );
}

#[test]
fn theme_action_persists_when_preference_path_is_configured() {
    let temp = tempfile::tempdir().expect("tempdir");
    let preference_path = temp.path().join(".euler").join("preferences.json");
    let mut core = core_with_provider_model_options_at(
        EchoProvider,
        "echo",
        ".",
        AppOptions {
            theme_preference_path: Some(preference_path.clone()),
            ..AppOptions::default()
        },
    );

    assert_eq!(
        core.set_theme(ThemeChoice::GruvboxLight),
        CoreEffect::ThemeChanged
    );

    let contents = std::fs::read_to_string(preference_path).expect("theme preference");
    let value: serde_json::Value = serde_json::from_str(&contents).expect("json");
    assert_eq!(value["theme"], "gruvbox-light");
}

#[test]
fn theme_action_reports_preference_save_failure_without_reverting_theme() {
    let temp = tempfile::tempdir().expect("tempdir");
    let preference_path = temp.path().join("preferences.json");
    std::fs::write(&preference_path, r#"{"theme":"light","#).expect("write malformed");
    let mut core = core_with_provider_model_options_at(
        EchoProvider,
        "echo",
        ".",
        AppOptions {
            theme_preference_path: Some(preference_path.clone()),
            ..AppOptions::default()
        },
    );

    assert_eq!(
        core.set_theme(ThemeChoice::GruvboxLight),
        CoreEffect::ThemeChanged
    );

    assert_eq!(core.theme_choice, ThemeChoice::GruvboxLight);
    assert!(drain_finalized_visual_text(&mut core, 80).contains("preference not saved"));
    assert_eq!(
        std::fs::read_to_string(&preference_path).expect("read unchanged"),
        r#"{"theme":"light","#
    );
}

#[test]
fn launch_auth_file_override_threads_into_the_core_for_resume_seeding() {
    // Secrets contract seeding gap: in-app resume re-seeds secret redaction
    // and must consult the SAME `--auth-file` the launch used — before this
    // threading, build_tui_resume fell back to the default auth store and
    // silently dropped the override's credential values from redaction.
    let core = core_with_provider_model_options_at(
        EchoProvider,
        "echo",
        ".",
        AppOptions {
            auth_file: Some(PathBuf::from("/tmp/custom-auth.json")),
            ..AppOptions::default()
        },
    );
    assert_eq!(
        core.auth_file.as_deref(),
        Some(std::path::Path::new("/tmp/custom-auth.json"))
    );
}

#[test]
fn resume_picker_reports_empty_state_without_active_turn_language() {
    let mut core = core();
    core.state = AppState::Empty;

    assert_eq!(
        core.resume_session_from_picker("01KW3M3QN4JYHPW1Y82VW9K7K1".to_owned()),
        CoreEffect::Render
    );

    let text = drain_finalized_visual_text(&mut core, 80);
    assert!(text.contains("resume needs an active session"));
    assert!(
        !text.contains("ui:"),
        "resume refusal is a neutral notice, not a \"ui:\" error: {text}"
    );
}

#[test]
fn resume_refusal_for_already_active_session_is_a_neutral_notice() {
    let mut core = core();
    let session_id = match &core.state {
        AppState::Idle { session } => session.session_id().to_owned(),
        _ => panic!("expected an idle session"),
    };

    assert_eq!(
        core.resume_session_from_picker(session_id.clone()),
        CoreEffect::Render
    );

    let text = drain_finalized_visual_text(&mut core, 80);
    assert!(text.contains(&format!("already using session {session_id}")));
    assert!(
        !text.contains("ui:"),
        "resume refusal is a neutral notice, not a \"ui:\" error: {text}"
    );
}

#[test]
fn resume_picker_waits_while_turn_is_in_flight() {
    let mut core = core();
    core.bottom.composer_mut().insert_text("keep this draft");
    let (_worker_tx, worker_rx) = std::sync::mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    assert_eq!(
        core.resume_session_from_picker("01KW3M3QN4JYHPW1Y82VW9K7K1".to_owned()),
        CoreEffect::Render
    );

    // Spec §5.10: faint notice above composer; input preserved (not a transcript error).
    assert_eq!(
        core.notice.as_deref(),
        Some("resume waits for the active turn")
    );
    assert_eq!(core.bottom.composer().submit_text(), "keep this draft");
    let text = drain_finalized_visual_text(&mut core, 80);
    assert!(
        text.contains("resume waits for the active turn"),
        "notice must render above composer: {text}"
    );
    assert!(
        !text.contains("! ui: resume waits"),
        "must not be a transcript error row: {text}"
    );
}

#[test]
fn accepting_resume_purges_prior_native_scrollback() {
    let mut core = core();
    let (decider, channels) = TuiDecider::new();
    let mut config = euler_core::SessionConfig::new(".");
    config.session_id = "01KW3Q6NN5A9R6E2EWZ7M3QW9T".to_owned();
    config.model = "echo".to_owned();
    config.agent_id = "resumed-owner".to_owned();
    let session = Session::new(config, EchoProvider, decider);
    let events = vec![event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "resumed content".into())]),
    )];

    let effect = core.accept_tui_resume(
        "01KW3Q6NN5A9R6E2EWZ7M3QW9T".to_owned(),
        TuiResume {
            session,
            channels,
            events,
            active_target: ModelTarget::new("fixture", "echo"),
            display_label: "useful resumed name".to_owned(),
            session_name: None,
            recovery_closure_appended: false,
            warning_count: 0,
            events_replayed: 1,
        },
    );

    assert_eq!(effect, CoreEffect::ReplayHistoryWithScrollbackPurge);
    assert_eq!(core.primary_agent_id.as_deref(), Some("resumed-owner"));
    assert!(core.notice.is_none());
    let text = drain_finalized_visual_text(&mut core, 80);
    assert!(text.contains("resumed content"), "text: {text}");
    assert!(
        text.contains("✓ resumed session useful resumed name"),
        "text: {text}"
    );
    assert!(
        text.contains("1 events replayed · model context folded to stubs"),
        "text: {text}"
    );
}

#[test]
fn accepting_resume_restamps_replayed_history_when_timestamps_are_on() {
    // Review v2 §6: a rebuild (resume, new session, rollback) must stamp
    // every replayed event from its own provenance time — not leave the
    // gutter blank until new events arrive.
    let mut core = core_with_provider_model_options_at(
        EchoProvider,
        "echo",
        ".",
        AppOptions {
            show_timestamp_gutter: Some(true),
            ..AppOptions::default()
        },
    );
    let (decider, channels) = TuiDecider::new();
    let mut config = euler_core::SessionConfig::new(".");
    config.session_id = "01KW3Q6NN5A9R6E2EWZ7M3QW9T".to_owned();
    config.model = "echo".to_owned();
    let session = Session::new(config, EchoProvider, decider);
    let events = vec![event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "resumed content".into())]),
    )];

    core.accept_tui_resume(
        "01KW3Q6NN5A9R6E2EWZ7M3QW9T".to_owned(),
        TuiResume {
            session,
            channels,
            events,
            active_target: ModelTarget::new("fixture", "echo"),
            display_label: "useful resumed name".to_owned(),
            session_name: None,
            recovery_closure_appended: false,
            warning_count: 0,
            events_replayed: 1,
        },
    );

    let lines = core
        .drain_finalized_visual_lines(80)
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>();
    let row = lines
        .iter()
        .position(|line| line.contains("resumed content"))
        .expect("replayed row");
    let stamp: String = lines[row].chars().take(8).collect();
    assert!(
        looks_like_hh_mm_ss(&stamp),
        "replayed history should be restamped, not blank: {:?}",
        lines[row]
    );
}

#[test]
fn accepting_resume_boundary_includes_recovery_and_warnings() {
    let mut core = core();
    let (decider, channels) = TuiDecider::new();
    let mut config = euler_core::SessionConfig::new(".");
    config.session_id = "01KW3Q6NN5A9R6E2EWZ7M3QW9T".to_owned();
    config.model = "echo".to_owned();
    let session = Session::new(config, EchoProvider, decider);

    let effect = core.accept_tui_resume(
        "01KW3Q6NN5A9R6E2EWZ7M3QW9T".to_owned(),
        TuiResume {
            session,
            channels,
            events: Vec::new(),
            active_target: ModelTarget::new("fixture", "echo"),
            display_label: "broken tail".to_owned(),
            session_name: None,
            recovery_closure_appended: true,
            warning_count: 2,
            events_replayed: 7,
        },
    );

    assert_eq!(effect, CoreEffect::ReplayHistoryWithScrollbackPurge);
    let text = drain_finalized_visual_text(&mut core, 80);
    assert!(
        text.contains("✓ resumed session broken tail"),
        "text: {text}"
    );
    assert!(text.contains("recovery closure appended"), "text: {text}");
    assert!(text.contains("warnings"), "text: {text}");
    assert!(text.contains("· 2"), "text: {text}");
    assert!(
        text.contains("7 events replayed · model context folded to stubs"),
        "text: {text}"
    );
}

#[test]
fn copy_key_copies_last_visible_assistant_response() {
    let mut core = core();
    let writes = Arc::new(Mutex::new(Vec::new()));
    core.clipboard = Box::new(RecordingClipboard {
        writes: Arc::clone(&writes),
        result: Ok(()),
    });
    core.transcript.push_event(event(
        EventKind::MODEL_REASONING,
        object([
            ("provider", "fixture".into()),
            ("model", "echo".into()),
            ("fidelity", "raw".into()),
            ("content", "do not copy".into()),
        ]),
    ));
    core.transcript.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "copy me".into())]),
    ));

    assert_eq!(
        core.handle_input(modified_key(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )),
        CoreEffect::Render
    );

    assert_eq!(*writes.lock().expect("clipboard lock"), vec!["copy me"]);
    assert_eq!(
        core.notice.as_deref(),
        Some("copied last assistant response")
    );
}

#[test]
fn copy_falls_back_to_terminal_clipboard_effect_after_desktop_failure() {
    let mut core = core();
    let writes = Arc::new(Mutex::new(Vec::new()));
    core.clipboard = Box::new(RecordingClipboard {
        writes: Arc::clone(&writes),
        result: Err("xclip exited with a non-zero status".to_owned()),
    });
    core.transcript.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "copy through terminal".into())]),
    ));

    let effect = core.handle_input(modified_key(
        KeyCode::Char('C'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));

    assert_eq!(effect, CoreEffect::TerminalClipboard);
    assert_eq!(
        *writes.lock().expect("clipboard lock"),
        vec!["copy through terminal"]
    );
    assert!(core.notice.is_none());
    assert_eq!(
        core.pending_terminal_clipboard.as_deref(),
        Some("\x1b]52;c;Y29weSB0aHJvdWdoIHRlcm1pbmFs\x07")
    );
}

#[test]
fn shadowed_terminal_clipboard_payload_is_discarded() {
    let mut core = core();
    core.pending_terminal_clipboard = Some("\x1b]52;c;Y29weQ==\x07".to_owned());

    core.discard_terminal_clipboard_if_shadowed(CoreEffect::ReplayHistoryWithScrollbackPurge);

    assert!(core.pending_terminal_clipboard.is_none());
}

#[test]
fn terminal_clipboard_payload_stays_queued_for_terminal_effect() {
    let mut core = core();
    core.pending_terminal_clipboard = Some("\x1b]52;c;Y29weQ==\x07".to_owned());

    core.discard_terminal_clipboard_if_shadowed(CoreEffect::TerminalClipboard);

    assert_eq!(
        core.pending_terminal_clipboard.as_deref(),
        Some("\x1b]52;c;Y29weQ==\x07")
    );
}

#[test]
fn ctrl_shift_c_copies_in_flight_without_interrupting() {
    let mut core = core();
    let writes = Arc::new(Mutex::new(Vec::new()));
    let (_tx, worker_rx) = mpsc::channel();
    let interrupt_flag = Arc::new(AtomicBool::new(false));
    core.clipboard = Box::new(RecordingClipboard {
        writes: Arc::clone(&writes),
        result: Ok(()),
    });
    core.transcript.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "copy in flight".into())]),
    ));
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::clone(&interrupt_flag),
        started_at: Instant::now(),
    };

    assert_eq!(
        core.handle_input(modified_key(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )),
        CoreEffect::Render
    );

    assert_eq!(
        *writes.lock().expect("clipboard lock"),
        vec!["copy in flight"]
    );
    assert!(!interrupt_flag.load(Ordering::SeqCst));
}

#[test]
fn copy_without_assistant_response_shows_notice_without_copying() {
    let mut core = core();
    let writes = Arc::new(Mutex::new(Vec::new()));
    core.clipboard = Box::new(RecordingClipboard {
        writes: Arc::clone(&writes),
        result: Ok(()),
    });

    core.handle_input(modified_key(
        KeyCode::Char('C'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));

    assert!(writes.lock().expect("clipboard lock").is_empty());
    assert_eq!(
        core.notice.as_deref(),
        Some("no assistant response to copy")
    );
}

#[test]
fn copy_slash_command_routes_to_copy_action() {
    let mut core = core();
    let writes = Arc::new(Mutex::new(Vec::new()));
    core.clipboard = Box::new(RecordingClipboard {
        writes: Arc::clone(&writes),
        result: Ok(()),
    });
    core.transcript.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "from slash".into())]),
    ));

    core.handle_input(key(KeyCode::Char('/')));
    for ch in "cop".chars() {
        core.handle_input(key(KeyCode::Char(ch)));
    }
    core.handle_input(key(KeyCode::Enter));

    assert_eq!(*writes.lock().expect("clipboard lock"), vec!["from slash"]);
}

#[test]
fn slash_palette_backspace_corrects_input_before_confirm() {
    let mut core = core();

    core.handle_input(key(KeyCode::Char('/')));
    for ch in "effortx".chars() {
        core.handle_input(key(KeyCode::Char(ch)));
    }
    core.handle_input(key(KeyCode::Backspace));
    for ch in " large".chars() {
        core.handle_input(key(KeyCode::Char(ch)));
    }

    let BottomOwner::Palette(palette) = core.bottom.owner() else {
        panic!("palette should own surface");
    };
    assert_eq!(palette.input(), "/effort large");

    core.handle_input(key(KeyCode::Enter));

    // #53: setting confirmations are neutral notices, not "ui:" errors.
    assert!(drain_finalized_visual_text(&mut core, 80).contains("reasoning effort set to large"));
}

#[test]
fn slash_palette_trailing_noise_correction_submits_visible_effort_argument() {
    let mut core = core();

    core.handle_input(key(KeyCode::Char('/')));
    for ch in "effort//dddf".chars() {
        core.handle_input(key(KeyCode::Char(ch)));
    }
    for _ in 0..4 {
        core.handle_input(key(KeyCode::Backspace));
    }
    for ch in " large".chars() {
        core.handle_input(key(KeyCode::Char(ch)));
    }

    let BottomOwner::Palette(palette) = core.bottom.owner() else {
        panic!("palette should own surface");
    };
    assert_eq!(palette.input(), "/effort// large");
    assert_eq!(palette.selected_token(), Some("/effort".to_owned()));

    core.handle_input(key(KeyCode::Enter));

    let pending = drain_finalized_visual_text(&mut core, 80);
    // #53: setting confirmations are neutral notices, not "ui:" errors.
    assert!(pending.contains("reasoning effort set to large"));
    assert!(!pending.contains("ui: reasoning effort set to large"));
    assert!(!pending.contains("unknown command: /effort//"));
}

#[test]
fn model_picker_uses_catalog_and_keeps_active_explicit_target() {
    let mut core = core_with_fixture_catalog(
        ChatGptEchoProvider,
        "gpt-5.5",
        MergedModelCatalog::built_in(),
    );

    core.handle_input(key(KeyCode::Char('/')));
    for ch in "model".chars() {
        core.handle_input(key(KeyCode::Char(ch)));
    }
    core.handle_input(key(KeyCode::Enter));

    let BottomOwner::Picker(picker) = core.bottom.owner() else {
        panic!("model picker should own surface");
    };
    let rendered = picker.render_lines(80).join("\n");
    // claude-fable-5 sorts first in the (now 14-entry) anthropic list, so it
    // is the anthropic model guaranteed inside the picker's render window.
    assert!(rendered.contains("anthropic::claude-fable-5 — 1M ctx, reasoning"));
    assert_eq!(picker.selected_index(), 0);

    core.handle_input(key(KeyCode::Down));
    let BottomOwner::Picker(picker) = core.bottom.owner() else {
        panic!("model picker should own surface");
    };
    assert_eq!(picker.selected_index(), 1);
    core.handle_input(key(KeyCode::Up));
    let BottomOwner::Picker(picker) = core.bottom.owner() else {
        panic!("model picker should own surface");
    };
    assert_eq!(picker.selected_index(), 0);

    for ch in "chatgpt".chars() {
        core.handle_input(key(KeyCode::Char(ch)));
    }
    let BottomOwner::Picker(picker) = core.bottom.owner() else {
        panic!("model picker should own surface");
    };
    let rendered = picker.render_lines(80).join("\n");
    assert!(
        rendered.contains("chatgpt::gpt-5.5 — 258K ctx, reasoning ✓"),
        "rendered picker:\n{rendered}"
    );
}

#[test]
fn model_switch_error_renders_as_ui_notice() {
    let mut core = core();

    assert_eq!(
        core.handle_command_action(CommandAction::SwitchModel {
            provider: "missing".to_owned(),
            model: "example-model".to_owned(),
        }),
        CoreEffect::Render
    );

    let rendered = drain_finalized_visual_text(&mut core, 200);
    assert!(rendered.contains("ui: model switch rejected:"));
    assert!(rendered.contains("missing"));
}

// Review v3 §R5(a): the recap and its `── Worked for Ns ──` divider are one
// unit — a turn too short to earn a divider must not leave an orphaned
// recap line either (observed live after error-only turns that finished
// under MIN_WORKED_DURATION).
#[test]
fn turn_recap_never_renders_without_its_worked_divider() {
    let mut core = core();
    core.transcript.push_event(event(
        EventKind::FILE_DIFF,
        object([("path", "src/lib.rs".into()), ("diff", "+line\n".into())]),
    ));

    core.handle_turn_outcome(TurnOutcome::Complete, Some(Duration::from_secs(1)));

    let text = drain_finalized_visual_text(&mut core, 80);
    assert!(
        !text.contains("Worked for"),
        "elapsed under MIN_WORKED_DURATION should suppress the divider: {text:?}"
    );
    // The recap would otherwise report the one changed file; its absence
    // (distinct from the persistent footer's own "ctx" token) confirms no
    // orphaned recap line rendered.
    assert!(
        !text.contains("1 file"),
        "recap must not render without its divider: {text:?}"
    );
}

// A turn that changed no files renders the divider (there was elapsed time
// worth naming) but no recap line — the rule is just "0 files", with no ctx
// or test conditions (owner preference, superseding review v3 §R5(b)).
#[test]
fn zero_file_turn_suppresses_recap_line_but_keeps_the_divider() {
    let mut core = core();
    core.handle_turn_outcome(TurnOutcome::Complete, Some(Duration::from_secs(10)));

    let text = drain_finalized_visual_text(&mut core, 80);
    assert!(
        text.contains("Worked for"),
        "an elapsed turn still earns its divider: {text:?}"
    );
    // "0 files" is the recap's own distinctive token (the footer's "ctx N%"
    // token alone isn't distinctive enough — it renders every turn).
    assert!(
        !text.contains("0 files"),
        "a turn that changed no files must suppress the recap line: {text:?}"
    );
}

/// Owner preference (2026-07-16): a turn that changed no files earns no
/// recap, even if a test-like command ran. Previously such a turn rendered
/// `0 files · tests …`; now the recap is suppressed and only the divider
/// remains. The test outcome is still visible in the tool output above.
#[test]
fn zero_file_turn_that_ran_tests_still_suppresses_the_recap() {
    let mut core = core();
    core.turn_event_start = 0;
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_CALL,
        object([
            ("id", "c1".into()),
            ("name", "run_shell".into()),
            ("input", serde_json::json!({"command": "cargo test -q"})),
        ]),
    )));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_RESULT,
        object([
            ("id", "c1".into()),
            ("name", "run_shell".into()),
            ("ok", true.into()),
            ("exit_code", 0.into()),
            ("output", "test result: ok. 3 passed; 0 failed".into()),
        ]),
    )));
    core.handle_turn_outcome(TurnOutcome::Complete, Some(Duration::from_secs(10)));

    let text = drain_finalized_visual_text(&mut core, 80);
    assert!(text.contains("Worked for"), "divider still shows: {text:?}");
    assert!(
        !text.contains("tests pass") && !text.contains("0 files"),
        "no files changed, so no recap line even with tests: {text:?}"
    );
}

#[test]
fn cancel_outcome_clears_transient_live_tail_before_next_turn() {
    let mut core = core();
    core.transcript.push_event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "stale".into())]),
    ));

    core.handle_turn_outcome(TurnOutcome::Cancelled, None);
    core.transcript.push_event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "fresh\n".into())]),
    ));

    assert!(core
        .transcript
        .items()
        .contains(&TranscriptItem::AssistantMessage("fresh\n".to_owned())));
    assert!(!core
        .transcript
        .items()
        .contains(&TranscriptItem::AssistantMessage("stalefresh".to_owned())));
}

#[test]
fn activity_live_status_is_gated_by_turn_state() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    core.transcript.push_event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "partial".into())]),
    ));

    terminal
        .draw(|frame| core.render(frame))
        .expect("idle draw");
    assert!(!terminal.backend().screen_contents().contains("⠋ working"));

    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    terminal
        .draw(|frame| core.render(frame))
        .expect("in-flight draw");
    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("⠋ working · 0s · esc to interrupt"));
    assert!(contents.contains("▌"));
}

/// Issue #27: the spinner frame is a pure tick counter, advanced only by
/// `advance_spinner` (the periodic background poll) never by reading
/// `Instant::now()` fresh in render — so the animation is testable by
/// injecting synthetic `now` values instead of sleeping.
#[test]
fn working_hud_spinner_advances_on_tick_not_wall_clock() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    let t0 = Instant::now();

    // First call only seeds the schedule; no tick fires yet.
    assert!(!core.advance_spinner_at(t0));
    assert_eq!(core.spinner_frame, 0);

    // Under the 90ms cadence: no advance.
    assert!(!core.advance_spinner_at(t0 + Duration::from_millis(50)));
    assert_eq!(core.spinner_frame, 0);

    // At/after the cadence: advances exactly one frame.
    assert!(core.advance_spinner_at(t0 + Duration::from_millis(90)));
    assert_eq!(core.spinner_frame, 1);
    assert!(!core.advance_spinner_at(t0 + Duration::from_millis(120)));
    assert_eq!(core.spinner_frame, 1);
    assert!(core.advance_spinner_at(t0 + Duration::from_millis(200)));
    assert_eq!(core.spinner_frame, 2);
}

#[test]
fn working_hud_spinner_resets_when_turn_ends() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    let t0 = Instant::now();
    core.advance_spinner_at(t0);
    core.advance_spinner_at(t0 + Duration::from_millis(90));
    assert_eq!(core.spinner_frame, 1);

    core.state = AppState::Empty;
    assert!(core.advance_spinner());
    assert_eq!(core.spinner_frame, 0);
}

/// Issue #27: the phase verb swaps in place as streamed tool-call/reasoning
/// events arrive, with `run_shell` distinguishing bash from a test-runner
/// invocation, and falls back to "working" before any such event lands.
#[test]
fn working_hud_phase_verb_reflects_streamed_turn_events() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    assert_eq!(core.current_phase_verb, None);

    // "thinking" comes from the live reasoning DELTAS — deltas arrive before
    // the finalized MODEL_REASONING event, so this is the moment the model
    // is actually thinking.
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "reasoning".into()), ("delta", "hmm".into())]),
    )));
    assert_eq!(core.current_phase_verb.as_deref(), Some("thinking"));

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_CALL,
        object([
            ("id", "call-read".into()),
            ("name", "read_file".into()),
            ("input", json!({"path": "src/lib.rs"})),
        ]),
    )));
    assert_eq!(
        core.current_phase_verb.as_deref(),
        Some("reading src/lib.rs")
    );

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_CALL,
        object([
            ("id", "call-edit".into()),
            ("name", "edit_file".into()),
            ("input", json!({"path": "src/lib.rs"})),
        ]),
    )));
    assert_eq!(
        core.current_phase_verb.as_deref(),
        Some("writing src/lib.rs")
    );

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_CALL,
        object([
            ("id", "call-bash".into()),
            ("name", "run_shell".into()),
            ("input", json!({"command": "ls -la"})),
        ]),
    )));
    assert_eq!(core.current_phase_verb.as_deref(), Some("running bash"));

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_CALL,
        object([
            ("id", "call-test".into()),
            ("name", "run_shell".into()),
            ("input", json!({"command": "cargo nextest run --workspace"})),
        ]),
    )));
    assert_eq!(core.current_phase_verb.as_deref(), Some("running tests"));

    // A non-phase-carrying event (model delta) leaves the verb in place.
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "answer".into())]),
    )));
    assert_eq!(core.current_phase_verb.as_deref(), Some("running tests"));
}

/// #62: the verb must not go stale once its tool call terminates — success,
/// failure, *or* auto-denial via the turn denial cache all resolve through
/// the same `tool.result` event, so all three must clear the verb back to
/// the live phase instead of parroting a tool that already finished.
#[test]
fn working_hud_phase_verb_clears_when_tool_call_terminates_any_way() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_CALL,
        object([
            ("id", "call-bash".into()),
            ("name", "run_shell".into()),
            ("input", json!({"command": "ls -la"})),
        ]),
    )));
    assert_eq!(core.current_phase_verb.as_deref(), Some("running bash"));

    // Auto-denied via the turn denial cache: `ok: false`, no distinct
    // tool-call event precedes it — same shape as a normal failure result.
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_RESULT,
        object([
            ("id", "call-bash".into()),
            ("name", "run_shell".into()),
            ("ok", false.into()),
            ("error", "permission denied".into()),
        ]),
    )));
    assert_eq!(
        core.current_phase_verb, None,
        "verb must fall back to the live phase once the tool call resolves, denied or not"
    );

    // A second bash attempt that succeeds also clears on its own result.
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_CALL,
        object([
            ("id", "call-bash-2".into()),
            ("name", "run_shell".into()),
            ("input", json!({"command": "ls -la"})),
        ]),
    )));
    assert_eq!(core.current_phase_verb.as_deref(), Some("running bash"));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_RESULT,
        object([
            ("id", "call-bash-2".into()),
            ("name", "run_shell".into()),
            ("ok", true.into()),
        ]),
    )));
    assert_eq!(core.current_phase_verb, None);
}

/// Ownership: reasoning TEXT renders only in the transcript's live card
/// behind the hairline; the HUD is a single status line carrying the sole
/// esc affordance. Event order here is the REAL one — reasoning deltas
/// first, the finalized `MODEL_REASONING` after — so the HUD must show
/// `thinking · Ns` DURING streaming, not only after finalize.
#[test]
fn hud_shows_one_line_thinking_status_during_reasoning_deltas() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_DELTA,
        object([
            ("kind", "reasoning".into()),
            ("delta", "weighing the residue lemma".into()),
        ]),
    )));
    assert_eq!(core.current_phase_verb.as_deref(), Some("thinking"));
    let status = core.live_status_line().expect("working HUD line");
    assert!(status.contains("thinking"), "{status}");
    assert!(status.contains("esc to interrupt"), "{status}");

    let frame = core.render_visual_canvas(80);
    let lines: Vec<String> = frame
        .active_frame_lines()
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect();
    let text = lines.join("\n");

    // The streamed reasoning text renders exactly once — the transcript's
    // live card — never duplicated under the HUD.
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("weighing the residue lemma"))
            .count(),
        1,
        "{text}"
    );
    // The esc affordance is advertised exactly once, on the HUD status
    // line; the transcript's live thinking header carries the timer only.
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("esc to interrupt"))
            .count(),
        1,
        "{text}"
    );
    let transcript_header = lines
        .iter()
        .find(|line| line.contains("thinking ·") && !line.contains("esc"))
        .expect("transcript live thinking header without an esc hint");
    assert!(
        !transcript_header.contains("weighing"),
        "the header is the timer line, not the body: {transcript_header}"
    );
    let hud_line = lines
        .iter()
        .find(|line| line.contains("esc to interrupt"))
        .expect("HUD status line");
    assert!(
        !hud_line.contains("residue lemma"),
        "the HUD must not carry reasoning text: {hud_line}"
    );

    // Finalize collapses the transcript card to the committed gist and
    // clears the HUD thinking status back to the "working" fallback.
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_REASONING,
        object([
            ("fidelity", "raw".into()),
            (
                "content",
                "weighing the residue lemma against the tower".into(),
            ),
        ]),
    )));
    assert_eq!(core.current_phase_verb, None);
    let status = core.live_status_line().expect("working HUD line");
    assert!(status.contains("working"), "{status}");

    let frame = core.render_visual_canvas(80);
    let text = frame
        .active_frame_lines()
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("thought for"), "{text}");
    assert!(!text.contains("thinking ·"), "{text}");
}

/// The HUD thinking status clears the moment answer text starts streaming
/// — while streamed text deltas leave a tool-phase verb alone (issue #27:
/// no mid-phase flicker).
#[test]
fn hud_thinking_status_clears_when_answer_text_starts() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "reasoning".into()), ("delta", "hmm".into())]),
    )));
    assert_eq!(core.current_phase_verb.as_deref(), Some("thinking"));

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "answer".into())]),
    )));
    assert_eq!(core.current_phase_verb, None);

    // A tool phase set later is NOT clobbered by further text deltas.
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::TOOL_CALL,
        object([
            ("id", "call-read".into()),
            ("name", "read_file".into()),
            ("input", json!({"path": "src/lib.rs"})),
        ]),
    )));
    assert_eq!(
        core.current_phase_verb.as_deref(),
        Some("reading src/lib.rs")
    );
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", "more".into())]),
    )));
    assert_eq!(
        core.current_phase_verb.as_deref(),
        Some("reading src/lib.rs")
    );
}

#[test]
fn working_hud_phase_verb_resets_when_a_new_turn_spawns() {
    let mut core = core();
    let AppState::Idle { session } = std::mem::replace(&mut core.state, AppState::Empty) else {
        panic!("core should start idle");
    };
    core.current_phase_verb = Some("thinking".to_owned());
    core.spinner_frame = 3;

    core.spawn_turn("next".to_owned(), session);

    assert_eq!(core.current_phase_verb, None);
    assert_eq!(core.spinner_frame, 0);
}

#[test]
fn working_hud_phase_verb_resets_when_the_turn_completes() {
    let mut core = core();
    let AppState::Idle { session } = std::mem::replace(&mut core.state, AppState::Empty) else {
        panic!("core should start idle");
    };
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    core.current_phase_verb = Some("thinking".to_owned());
    core.spinner_frame = 3;

    core.handle_turn_event(TurnEvent::TurnDone {
        outcome: TurnOutcome::Complete,
        session,
    });

    assert_eq!(core.current_phase_verb, None);
    assert_eq!(core.spinner_frame, 0);
}

/// Issue #27: the spinner is gold (the theme's warning token) and the
/// elapsed/hint suffix is dim (muted) — both routed through `Theme`, not a
/// hardcoded hex; the verb itself carries no explicit color.
#[test]
fn working_hud_canvas_line_uses_theme_tokens_for_spinner_and_suffix() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    core.theme = Theme::warm_ledger();

    let frame = core.render_visual_canvas(80);
    let frame_lines = frame.active_frame_lines();
    let activity_line = frame_lines
        .iter()
        .find(|line| line.plain_text().contains("working"))
        .expect("activity line present");
    assert_eq!(activity_line.spans.len(), 3);
    assert_eq!(
        activity_line.spans[0].style.fg,
        Some(core.theme.palette.warning)
    );
    assert_eq!(activity_line.spans[1].style.fg, None);
    assert_eq!(
        activity_line.spans[2].style.fg,
        Some(core.theme.palette.muted)
    );
}

/// Issue #27: the HUD line sits directly above the composer with no blank
/// line between them.
#[test]
fn working_hud_sits_directly_above_composer_with_no_blank_line() {
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };

    let frame = core.render_visual_canvas(80);
    let lines = frame
        .active_frame_lines()
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>();
    let hud_row = lines
        .iter()
        .position(|line| line.contains("working"))
        .expect("HUD row present");
    let composer_row = lines
        .iter()
        .enumerate()
        .skip(hud_row + 1)
        .find(|(_, line)| line.starts_with('▌'))
        .map(|(index, _)| index)
        .expect("composer row present");
    assert_eq!(
        composer_row,
        hud_row + 1,
        "no blank row between the HUD and the composer: lines: {lines:?}"
    );
}

#[test]
fn interrupted_live_status_replaces_working_affordance() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    let (_tx, worker_rx) = mpsc::channel();
    let interrupt_flag = Arc::new(AtomicBool::new(false));
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::clone(&interrupt_flag),
        started_at: Instant::now(),
    };

    assert_eq!(core.handle_interrupt(), CoreEffect::Render);
    assert!(interrupt_flag.load(Ordering::SeqCst));

    terminal.draw(|frame| core.render(frame)).expect("draw");
    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("■ interrupted — tell euler what to do differently"));
    assert!(!contents.contains("⠋ working"));
}

#[test]
fn layout_renders_at_80_by_24_and_after_resize() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    core.handle_input(key(KeyCode::Char('/')));
    terminal.draw(|frame| core.render(frame)).expect("draw");
    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("> /"));
    let areas = layout(
        Rect::new(0, 0, 80, 24),
        core.composer_height(),
        core.notice_height(),
        core.permission_ask_height(80),
        core.activity_height(),
    );
    let frame_top = areas.bottom.y + areas.bottom.height - core.composer_frame_height(7, 80);
    assert!(!screen_row(&contents, frame_top).contains('\u{2500}'));
    assert!(contents.contains("▌"));

    terminal.backend_mut().resize(120, 40);
    assert!(terminal.draw(|frame| core.render(frame)).is_ok());
    let resized = terminal.backend().screen_contents();
    assert!(resized.contains("echo(medium) · ctx ?%"));
    assert!(!resized.contains("Context ?% used"));
}

#[test]
fn footer_context_is_zero_percent_at_fresh_session_with_known_window() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core_with_fixture_catalog(
        EchoProvider,
        "echo",
        fixture_catalog_with_windows(&[("echo", 1_000)]),
    );

    terminal.draw(|frame| core.render(frame)).expect("draw");

    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("ctx 0%"));
    assert!(!contents.contains("ctx ?%"));
    assert!(!contents.contains("Context ?% used"));
}

#[test]
fn footer_context_is_unknown_when_model_window_is_unknown() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core =
        core_with_fixture_catalog(EchoProvider, "echo", fixture_catalog_with_windows(&[]));

    terminal.draw(|frame| core.render(frame)).expect("draw");

    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("ctx ?%"));
}

#[test]
fn scripted_model_result_usage_updates_footer_context_percent() {
    let provider = ScriptedProvider::new(vec![scripted_usage(123)]);
    let mut core = core_with_fixture_catalog(
        provider,
        "echo",
        fixture_catalog_with_windows(&[("echo", 1_000)]),
    );
    core.status.cwd = PathBuf::from("/tmp/euler");

    core.handle_input(key(KeyCode::Char('e')));
    core.handle_input(key(KeyCode::Enter));
    wait_for_idle(&mut core);

    let rendered = core.canvas_status_snapshot(120).line.plain_text();
    // Footer v2 (#48): two hard-edged clusters, empty middle, no branch on
    // the right (there is none here — non-git fixture cwd) and no `?` fill.
    assert_eq!(
        rendered,
        format!(
            "  / commands · /tmp/euler{}echo(medium) · ctx 12%",
            " ".repeat(72)
        )
    );
    assert_eq!(core.token_usage.input_tokens, 123);
    assert_eq!(core.token_usage.output_tokens, 999);
    assert_eq!(core.token_usage.reasoning_tokens, Some(500));
}

#[test]
fn persisted_model_results_rebuild_footer_cost_on_resume() {
    let (catalog, warnings) = MergedModelCatalog::with_local_json(
        r#"{
          "providers": {"fixture": {"models": [{
            "id": "echo",
            "context_window_tokens": 1000,
            "cost": {"input": 999, "output": 999}
          }]}}
        }"#,
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut core = core_with_fixture_catalog(EchoProvider, "echo", catalog);
    let events = vec![model_result_usage_and_cost_event(
        json!({
            "input_tokens": 1_000,
            "output_tokens": 100,
            "cached_tokens": 400
        }),
        6_200_000_000,
    )];

    core.rebuild_transcript_from_events(&events);

    assert_eq!(core.token_usage.session_cost_picos, 6_200_000_000);
    assert_eq!(core.token_usage.priced_calls, 1);
    assert!(core
        .canvas_status_snapshot(120)
        .line
        .plain_text()
        .ends_with("echo(medium) · ctx 99% · $0.006"));
}

#[test]
fn persisted_cost_rebuild_keeps_footer_subtotal_numeric_for_mixed_history() {
    let catalog = fixture_catalog_with_windows(&[("echo", 1_000)]);
    let mut core = core_with_fixture_catalog(EchoProvider, "echo", catalog);
    let events = vec![
        model_result_usage_and_cost_event(
            json!({"input_tokens": 100, "output_tokens": 10}),
            6_200_000_000,
        ),
        model_result_usage_event(json!({"input_tokens": 200, "output_tokens": 20})),
    ];

    core.rebuild_transcript_from_events(&events);

    assert_eq!(core.token_usage.session_cost_picos, 6_200_000_000);
    assert_eq!(core.token_usage.priced_calls, 1);
    assert_eq!(core.token_usage.unpriced_calls, 1);
    assert!(core
        .canvas_status_snapshot(120)
        .line
        .plain_text()
        .ends_with("echo(medium) · ctx 20% · $0.006"));
    assert!(
        format_session_usage(&events, &core.status, &core.token_usage)
            .starts_with("usage · session totals · $0.006200+ (1 unpriced call(s))")
    );
}

#[test]
fn detailed_usage_distinguishes_unpriced_history_from_zero_cost() {
    assert_eq!(usage_cost_text(0, 0, 1), "$? (1 unpriced call(s))");
    assert_eq!(
        usage_cost_text(6_200_000_000, 1, 2),
        "$0.006200+ (2 unpriced call(s))"
    );
}

#[test]
fn model_switch_resets_footer_context_until_next_result() {
    let mut core = core_with_fixture_catalog(
        EchoProvider,
        "echo",
        fixture_catalog_with_windows(&[("echo", 1_000), ("other", 2_000)]),
    );
    core.status.cwd = PathBuf::from("/tmp/euler");

    core.handle_turn_event(TurnEvent::Event(model_result_usage_event(json!({
        "input_tokens": 120,
        "output_tokens": 0
    }))));
    assert_eq!(
        core.canvas_status_snapshot(120).line.plain_text(),
        format!(
            "  / commands · /tmp/euler{}echo(medium) · ctx 12%",
            " ".repeat(72)
        )
    );

    core.status.model = "other".to_owned();
    core.handle_turn_event(TurnEvent::Event(model_switched_event("echo", "other")));
    assert_eq!(
        core.canvas_status_snapshot(120).line.plain_text(),
        format!(
            "  / commands · /tmp/euler{}other(medium) · ctx 0%",
            " ".repeat(72)
        )
    );

    core.handle_turn_event(TurnEvent::Event(model_result_usage_event_for_model(
        "other",
        json!({"input_tokens": 250, "output_tokens": 0}),
    )));
    assert_eq!(
        core.canvas_status_snapshot(120).line.plain_text(),
        format!(
            "  / commands · /tmp/euler{}other(medium) · ctx 13%",
            " ".repeat(71)
        )
    );
}

#[test]
fn patch_approval_modal_renders_diff_and_prompt() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    core.modal = Some(patch_modal(diff_preview("alpha\n", "beta\n")));

    terminal.draw(|frame| core.render(frame)).expect("draw");

    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("Edit file?"));
    assert!(!contents.contains("Approval required"));
    assert!(contents.contains("fs-write · cwd"));
    assert!(contents.contains("note.txt"));
    assert!(contents.contains("alpha"));
    assert!(contents.contains("beta"));
    let visual = core
        .visual_canvas_frame(80)
        .active_frame_lines()
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        visual.contains("write scope note.txt"),
        "visual: {visual:?}"
    );
    // v2.1 (§7b): unknown/zero fields are omitted, not padded with "ran-before 0×".
    assert!(!visual.contains("ran-before"), "visual: {visual:?}");
    assert!(contents.contains("y  Allow once"));
    assert!(!contents.contains("(default selection)"));
    assert!(contents.contains("a  Allow fs-write"));
    assert!(contents.contains("p  Allow fs-write"));
    assert!(contents.contains("n/esc  Deny"));
    assert!(contents.contains("Deny with instructions"));
    assert!(!contents.contains("hint: every decision is logged"));
    assert!(!contents.contains("commands that start"));
}

#[test]
fn patch_approval_modal_has_blank_line_before_options_and_gold_selection() {
    let mut core = core();
    core.modal = Some(patch_modal(diff_preview("alpha\n", "beta\n")));

    let lines = core
        .visual_canvas_frame(80)
        .active_frame_lines()
        .into_iter()
        .collect::<Vec<_>>();
    let plain = lines
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>();

    let options_row = plain
        .iter()
        .position(|line| line.contains("y  Allow once"))
        .expect("options row present");
    assert!(
        plain[options_row - 1].trim().is_empty()
            || plain[options_row - 1].trim_matches(['│', ' ']).is_empty(),
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
fn patch_approval_modal_clears_full_rows_behind_modal() {
    let mut terminal = Terminal::new(VT100Backend::new(120, 24)).expect("terminal");
    let mut core = core();
    core.transcript.push_event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "Model chatg ".repeat(80).into())]),
    ));
    core.modal = Some(patch_modal(diff_preview("alpha\n", "beta\n")));

    terminal.draw(|frame| core.render(frame)).expect("draw");

    let contents = terminal.backend().screen_contents();
    for (needle, expected) in [
        ("Edit file?", "Edit file?"),
        ("fs-write · cwd", "fs-write · cwd"),
        ("y  Allow once", "y  Allow once"),
        ("n/esc  Deny", "n/esc  Deny"),
    ] {
        let line = contents
            .lines()
            .find(|line| line.contains(needle))
            .expect(needle);
        assert!(line.contains(expected));
    }
}

#[test]
fn patch_review_key_no_longer_expands_diff_region() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    let new = (0..40)
        .map(|index| format!("line {index:02}\n"))
        .collect::<String>();
    core.modal = Some(patch_modal(diff_preview("", &new)));

    terminal.draw(|frame| core.render(frame)).expect("draw");
    let normal = terminal.backend().screen_contents();
    assert!(!normal.contains("line 10"));

    // `r` is no longer an expand key; nearest-block ctrl+o owns expansion.
    let _ = core.handle_input(key(KeyCode::Char('r')));
    terminal
        .draw(|frame| core.render(frame))
        .expect("draw after r");

    let after = terminal.backend().screen_contents();
    assert!(
        !after.contains("line 10"),
        "r must not expand patch modal: {after:?}"
    );
    assert!(matches!(
        core.modal,
        Some(Modal::PatchApproval(PatchApprovalModal { .. }))
    ));
}

#[test]
fn malformed_patch_payload_renders_fallback() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    core.transcript.push_event(event(
        EventKind::TOOL_CALL,
        object([
            ("id", "call-edit".into()),
            ("name", "edit_file".into()),
            ("input", json!({})),
        ]),
    ));
    core.modal = Some(core.modal_for_request(fs_write_request()));

    terminal.draw(|frame| core.render(frame)).expect("draw");

    assert!(terminal
        .backend()
        .screen_contents()
        .contains("Patch details are malformed or empty."));
}

#[test]
fn apply_patch_payload_renders_patch_preview() {
    let mut terminal = Terminal::new(VT100Backend::new(96, 24)).expect("terminal");
    let mut core = core();
    core.transcript.push_event(event(
        EventKind::TOOL_CALL,
        object([
            ("id", "call-patch".into()),
            ("name", "apply_patch".into()),
            (
                "input",
                json!({
                    "patch": "*** Begin Patch\n*** Add File: complex_50.c\n+#include <stdio.h>\n+int main(void){return 0;}\n*** End Patch"
                }),
            ),
        ]),
    ));
    core.modal = Some(core.modal_for_request(apply_patch_request()));

    terminal.draw(|frame| core.render(frame)).expect("draw");
    let contents = terminal.backend().screen_contents();

    assert!(!contents.contains("Patch details are malformed or empty."));
    assert!(contents.contains("Patch approval"));
    assert!(contents.contains("complex_50.c"));
    assert!(contents.contains("#include <stdio.h>"));
}

#[test]
fn large_patch_modal_keeps_diff_bounded() {
    let mut terminal = Terminal::new(VT100Backend::new(80, 24)).expect("terminal");
    let mut core = core();
    let new = (0..120)
        .map(|index| format!("line {index:03}\n"))
        .collect::<String>();
    core.modal = Some(patch_modal(diff_preview("", &new)));

    terminal.draw(|frame| core.render(frame)).expect("draw");

    let contents = terminal.backend().screen_contents();
    assert!(contents.contains("ctrl+o expand"));
    assert!(contents.contains("n/esc  Deny"));
    assert!(!contents.contains("line 119"));
}

#[test]
fn rejecting_patch_permission_does_not_apply_patch_and_turn_continues() {
    let temp = tempfile::tempdir().expect("temp dir");
    let note = temp.path().join("note.txt");
    std::fs::write(&note, "alpha\n").expect("write note");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-edit".to_owned(),
            name: "edit_file".to_owned(),
            input: json!({"path": "note.txt", "old": "alpha", "new": "beta"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut core = core_with_provider_at(provider, temp.path());

    core.handle_input(key(KeyCode::Char('e')));
    core.handle_input(key(KeyCode::Enter));
    wait_for_patch_diff(&mut core);
    assert!(matches!(core.modal, Some(Modal::PatchApproval(_))));
    assert_eq!(
        std::fs::read_to_string(&note).expect("read note"),
        "alpha\n"
    );

    core.handle_input(key(KeyCode::Char('n')));
    wait_for_idle(&mut core);

    assert_eq!(
        std::fs::read_to_string(&note).expect("read note"),
        "alpha\n"
    );
    assert!(core.transcript.events().iter().any(|event| {
        event.kind.as_str() == EventKind::PERMISSION_DECISION
            && event
                .payload
                .get("decision")
                .and_then(serde_json::Value::as_str)
                == Some("denied")
    }));
    let result = core
        .transcript
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .expect("tool result");
    assert!(result
        .payload
        .get("error")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|error| error.starts_with("permission denied")));
    assert!(core
        .transcript
        .items()
        .contains(&TranscriptItem::AssistantMessage("done".to_owned())));
    assert!(!core.transcript.items().iter().any(|item| {
        matches!(
            item,
            TranscriptItem::AssistantMessage(content)
                if content.contains("Permission was denied for")
        )
    }));
}

#[test]
fn allowing_patch_permission_applies_patch_and_turn_continues() {
    let temp = tempfile::tempdir().expect("temp dir");
    let note = temp.path().join("note.txt");
    std::fs::write(&note, "alpha\n").expect("write note");
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-edit".to_owned(),
            name: "edit_file".to_owned(),
            input: json!({"path": "note.txt", "old": "alpha", "new": "beta"}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut core = core_with_provider_at(provider, temp.path());

    core.handle_input(key(KeyCode::Char('e')));
    core.handle_input(key(KeyCode::Enter));
    wait_for_patch_diff(&mut core);
    assert!(matches!(
        &core.modal,
        Some(Modal::PatchApproval(PatchApprovalModal {
            preview: PatchPreview::Diff { new, .. },
            ..
        })) if new == "beta"
    ));

    core.handle_input(key(KeyCode::Char('y')));
    wait_for_idle(&mut core);

    assert_eq!(std::fs::read_to_string(&note).expect("read note"), "beta\n");
    assert!(core
        .transcript
        .events()
        .iter()
        .any(|event| event.kind.as_str() == EventKind::PATCH_APPLIED));
    assert!(core
        .transcript
        .items()
        .contains(&TranscriptItem::AssistantMessage("done".to_owned())));
}

#[test]
fn allowing_direct_apply_patch_permission_adds_file_and_turn_continues() {
    let temp = tempfile::tempdir().expect("temp dir");
    let patch = "*** Begin Patch\n*** Add File: complex_50.c\n+#include <stdio.h>\n+int main(void) { return 0; }\n*** End Patch";
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-apply".to_owned(),
            name: "apply_patch".to_owned(),
            input: json!({"patch": patch}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut core = core_with_provider_at(provider, temp.path());

    core.handle_input(key(KeyCode::Char('e')));
    core.handle_input(key(KeyCode::Enter));
    wait_for_patch_diff(&mut core);
    assert!(matches!(
        &core.modal,
        Some(Modal::PatchApproval(PatchApprovalModal {
            preview: PatchPreview::Diff { path, new, .. },
            ..
        })) if path == "complex_50.c" && new.contains("#include <stdio.h>")
    ));

    core.handle_input(key(KeyCode::Char('y')));
    wait_for_idle(&mut core);

    assert_eq!(
        std::fs::read_to_string(temp.path().join("complex_50.c")).expect("created file"),
        "#include <stdio.h>\nint main(void) { return 0; }\n"
    );
    assert!(core
        .transcript
        .items()
        .contains(&TranscriptItem::AssistantMessage("done".to_owned())));
}

#[test]
fn allowing_direct_apply_patch_permission_updates_file_and_turn_continues() {
    let temp = tempfile::tempdir().expect("temp dir");
    let note = temp.path().join("note.txt");
    std::fs::write(&note, "alpha\nmiddle\nomega\n").expect("write note");
    let patch = "*** Begin Patch\n*** Update File: note.txt\n@@\n-alpha\n+beta\n@@\n-omega\n+done\n*** End Patch";
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-apply-update".to_owned(),
            name: "apply_patch".to_owned(),
            input: json!({"patch": patch}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut core = core_with_provider_at(provider, temp.path());

    core.handle_input(key(KeyCode::Char('e')));
    core.handle_input(key(KeyCode::Enter));
    wait_for_patch_diff(&mut core);
    assert!(matches!(
        &core.modal,
        Some(Modal::PatchApproval(PatchApprovalModal {
            preview: PatchPreview::Diff { path, old, new },
            ..
        })) if path == "note.txt"
            && old.contains("alpha")
            && old.contains("omega")
            && new.contains("beta")
            && new.contains("done")
    ));

    core.handle_input(key(KeyCode::Char('y')));
    wait_for_idle(&mut core);

    assert_eq!(
        std::fs::read_to_string(&note).expect("updated file"),
        "beta\nmiddle\ndone\n"
    );
    assert!(core
        .transcript
        .items()
        .contains(&TranscriptItem::AssistantMessage("done".to_owned())));
}

#[test]
fn allowing_unsupported_direct_apply_patch_does_not_apply_patch() {
    let temp = tempfile::tempdir().expect("temp dir");
    let patch = "*** Begin Patch\n*** Add File: one.txt\n+one\n*** End Patch\n*** Add File: two.txt\n+two\n*** End Patch";
    let provider = ScriptedProvider::new(vec![
        FixtureResponse::ToolCalls(vec![ToolCall {
            id: "call-apply-unsupported".to_owned(),
            name: "apply_patch".to_owned(),
            input: json!({"patch": patch}),
        }]),
        FixtureResponse::Assistant("done".to_owned()),
    ]);
    let mut core = core_with_provider_at(provider, temp.path());

    core.handle_input(key(KeyCode::Char('e')));
    core.handle_input(key(KeyCode::Enter));
    wait_for_patch_modal(&mut core);
    assert!(matches!(
        &core.modal,
        Some(Modal::PatchApproval(PatchApprovalModal {
            preview: PatchPreview::Fallback(message),
            ..
        })) if message.contains("Patch preview unavailable")
    ));

    core.handle_input(key(KeyCode::Char('y')));
    wait_for_idle(&mut core);

    assert!(!temp.path().join("one.txt").exists());
    assert!(!temp.path().join("two.txt").exists());
    assert!(!core
        .transcript
        .events()
        .iter()
        .any(|event| event.kind.as_str() == EventKind::PATCH_APPLIED));
    assert!(core.transcript.events().iter().any(|event| {
        event.kind.as_str() == EventKind::TOOL_RESULT
            && event.payload.get("ok").and_then(serde_json::Value::as_bool) == Some(false)
    }));
}

fn event(kind: &'static str, payload: euler_event::JsonObject) -> EventEnvelope {
    EventEnvelope::new("session", "agent", None, kind, payload)
}

fn model_result_usage_event(usage: serde_json::Value) -> EventEnvelope {
    model_result_usage_event_for_model("echo", usage)
}

fn model_result_usage_and_cost_event(usage: serde_json::Value, total_picos: u64) -> EventEnvelope {
    let mut usage = usage;
    let usage = usage.as_object_mut().expect("usage object");
    let input_tokens = usage["input_tokens"].as_u64().expect("input tokens");
    let output_tokens = usage["output_tokens"].as_u64().expect("output tokens");
    let cached_tokens = usage
        .get("cached_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let uncached_input_tokens = input_tokens
        .checked_sub(cached_tokens)
        .expect("disjoint usage");
    usage.insert(
        "uncached_input_tokens".to_owned(),
        uncached_input_tokens.into(),
    );
    usage.insert("cached_tokens".to_owned(), cached_tokens.into());
    usage.insert("cache_write_5m_tokens".to_owned(), 0.into());
    usage.insert("cache_write_1h_tokens".to_owned(), 0.into());
    let output_rate = total_picos.checked_div(output_tokens).expect("output rate");
    assert_eq!(output_rate * output_tokens, total_picos);
    let mut event = model_result_usage_event(serde_json::Value::Object(usage.clone()));
    event.payload.insert(
        "cost".to_owned(),
        json!({
            "schema_version": 1,
            "currency": "USD",
            "unit": "picodollar",
            "input_picos": 0,
            "output_picos": total_picos,
            "cache_read_picos": 0,
            "cache_write_5m_picos": 0,
            "cache_write_1h_picos": 0,
            "total_picos": total_picos,
            "pricing": {
                "provider": "fixture",
                "model": "echo",
                "source": "local",
                "source_id": "0000000000000000000000000000000000000000000000000000000000000000",
                "rates": {
                    "input_picos_per_token": 0,
                    "output_picos_per_token": output_rate,
                    "cache_read_picos_per_token": 0
                }
            }
        }),
    );
    event
}

fn model_result_usage_event_for_model(model: &str, usage: serde_json::Value) -> EventEnvelope {
    primary_event(
        EventKind::MODEL_RESULT,
        object([
            ("provider", "fixture".into()),
            ("model", model.into()),
            ("content", "done".into()),
            ("tool_calls", json!([])),
            ("stop_reason", "completed".into()),
            ("usage", usage),
        ]),
    )
}

fn model_switched_event(from_model: &str, to_model: &str) -> EventEnvelope {
    primary_event(
        EventKind::MODEL_SWITCHED,
        object([
            ("from_provider", "fixture".into()),
            ("from_model", from_model.into()),
            ("to_provider", "fixture".into()),
            ("to_model", to_model.into()),
            ("reason", "user".into()),
        ]),
    )
}

fn primary_event(kind: &'static str, payload: euler_event::JsonObject) -> EventEnvelope {
    EventEnvelope::new("session", "root", None, kind, payload)
}

fn drain_finalized_visual_text(core: &mut AppCore, width: u16) -> String {
    core.drain_finalized_visual_lines(width)
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn ctrl_f_opens_read_only_transcript_search() {
    let mut core = core();
    assert_eq!(
        core.handle_input(modified_key(KeyCode::Char('f'), KeyModifiers::CONTROL)),
        CoreEffect::Render
    );
    assert!(matches!(core.bottom.owner(), BottomOwner::Search(_)));
    let status = core.canvas_status_snapshot(80);
    assert!(
        status.line.plain_text().contains("find:"),
        "search should replace footer hints: {}",
        status.line.plain_text()
    );

    core.handle_input(key(KeyCode::Char('x')));
    assert!(matches!(core.bottom.owner(), BottomOwner::Search(_)));
    // Search must not clear fold state.
    assert!(!core.tool_output_expanded);

    assert_eq!(core.handle_input(key(KeyCode::Esc)), CoreEffect::Render);
    assert!(matches!(core.bottom.owner(), BottomOwner::Composer));
    assert_eq!(core.visual_scroll_offset, 0);
}

#[test]
fn timestamps_toggle_persists_and_logs_confirmation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let preference_path = temp.path().join("preferences.json");
    let mut core = core_with_provider_model_options_at(
        EchoProvider,
        "echo",
        ".",
        AppOptions {
            theme_preference_path: Some(preference_path.clone()),
            show_timestamp_gutter: Some(true),
            ..AppOptions::default()
        },
    );

    assert!(core.show_timestamp_gutter);
    assert_eq!(
        core.handle_command_action(CommandAction::ToggleTimestamps),
        CoreEffect::Render
    );
    assert!(!core.show_timestamp_gutter);
    let text = drain_finalized_visual_text(&mut core, 80);
    assert!(
        text.contains("timestamps hidden"),
        "expected confirmation in transcript: {text}"
    );
    assert!(
        !text.contains("ui:"),
        "the /timestamps confirmation is a neutral notice, not a \"ui:\" error: {text}"
    );
    assert_eq!(
        crate::model_preference::load_timestamps_preference(&preference_path),
        crate::model_preference::TimestampsPreferenceLoad::Loaded(false)
    );

    assert_eq!(
        core.handle_command_action(CommandAction::ToggleTimestamps),
        CoreEffect::Render
    );
    assert!(core.show_timestamp_gutter);
}

#[test]
fn toggled_on_timestamp_gutter_stamps_every_event_first_row() {
    // Review v2 §6: the opt-in gutter must show every event's real
    // provenance time, not a blank column — this is the production
    // visual-canvas path (real EventEnvelope timestamps), not a bare
    // TranscriptItem fixture.
    let mut core = core_with_provider_model_options_at(
        EchoProvider,
        "echo",
        ".",
        AppOptions {
            show_timestamp_gutter: Some(true),
            ..AppOptions::default()
        },
    );
    core.drain_finalized_visual_lines(80);

    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::USER_MESSAGE,
        object([("content", "hi".into())]),
    )));
    core.handle_turn_event(TurnEvent::Event(event(
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "hello there".into())]),
    )));

    let lines = core
        .drain_finalized_visual_lines(80)
        .iter()
        .map(crate::ui::visual_canvas::CanvasLine::plain_text)
        .collect::<Vec<_>>();

    let user_row = lines
        .iter()
        .position(|line| line.contains("hi"))
        .expect("user row");
    let answer_row = lines
        .iter()
        .position(|line| line.contains("hello there"))
        .expect("answer row");

    for (label, row) in [("user", user_row), ("answer", answer_row)] {
        let line = &lines[row];
        let stamp: String = line.chars().take(8).collect();
        assert!(
            looks_like_hh_mm_ss(&stamp),
            "{label} row should stamp a real HH:MM:SS, not a blank gutter: {line:?}"
        );
    }
}

fn looks_like_hh_mm_ss(stamp: &str) -> bool {
    let chars: Vec<char> = stamp.chars().collect();
    chars.len() == 8
        && chars[0].is_ascii_digit()
        && chars[1].is_ascii_digit()
        && chars[2] == ':'
        && chars[3].is_ascii_digit()
        && chars[4].is_ascii_digit()
        && chars[5] == ':'
        && chars[6].is_ascii_digit()
        && chars[7].is_ascii_digit()
}

#[test]
fn at_mention_inserts_path_token_into_composer() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(temp.path().join("src")).expect("src");
    std::fs::write(temp.path().join("src/lib.rs"), "fn x() {}").expect("write");
    let mut core = core_with_provider_at(EchoProvider, temp.path());

    assert_eq!(
        core.handle_input(key(KeyCode::Char('@'))),
        CoreEffect::Render
    );
    assert!(matches!(core.bottom.owner(), BottomOwner::Mention(_)));

    // Narrow to the only file if needed, then confirm.
    for ch in "lib".chars() {
        core.handle_input(key(KeyCode::Char(ch)));
    }
    assert_eq!(core.handle_input(key(KeyCode::Enter)), CoreEffect::Render);
    assert!(matches!(core.bottom.owner(), BottomOwner::Composer));
    let text = core.bottom.composer().render_text();
    assert!(
        text.contains("lib.rs") || text.contains("@"),
        "composer should show mention path: {text:?}"
    );
    assert!(
        !core.bottom.composer().mentioned_paths().is_empty()
            || core.bottom.composer().submit_text().contains("lib.rs"),
        "mention should attach path for submit"
    );
}

fn wait_for_patch_diff(core: &mut AppCore) {
    for _ in 0..100 {
        core.drain_background();
        if matches!(
            core.modal,
            Some(Modal::PatchApproval(PatchApprovalModal {
                preview: PatchPreview::Diff { .. },
                ..
            }))
        ) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("patch diff did not appear");
}

fn wait_for_permission_modal(core: &mut AppCore) {
    for _ in 0..100 {
        core.drain_background();
        if matches!(core.modal, Some(Modal::Permission(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("permission modal did not appear");
}

fn wait_for_patch_modal(core: &mut AppCore) {
    for _ in 0..100 {
        core.drain_background();
        if matches!(core.modal, Some(Modal::PatchApproval(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("patch modal did not appear");
}

fn wait_for_idle(core: &mut AppCore) {
    for _ in 0..100 {
        core.drain_background();
        if !core.turn_in_flight() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("turn did not finish");
}

#[test]
fn code_swarm_picker_choices_survive_rebuild_while_turn_in_flight() {
    let mut core = core();
    let idle_choices = core.bottom.context().code_swarm_model_choices.clone();
    assert!(
        !idle_choices.is_empty(),
        "idle session with an authenticated provider offers reviewer targets"
    );

    // handle_submit rebuilds the bottom surface AFTER checking the session
    // out onto the worker thread; simulate that state and rebuild again.
    let (_tx, worker_rx) = mpsc::channel();
    core.state = AppState::TurnInFlight {
        worker_rx,
        interrupt_flag: Arc::new(AtomicBool::new(false)),
        started_at: Instant::now(),
    };
    core.rebuild_bottom_surface();

    assert_eq!(
        core.bottom.context().code_swarm_model_choices,
        idle_choices,
        "the reviewer-model picker must never shrink because a turn is in flight"
    );
}
