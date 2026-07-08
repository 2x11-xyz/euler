#[cfg(test)]
use super::app_layout::{layout, string_lines};
use super::bottom_surface::{BottomOwner, BottomSurface, SurfaceEvent};
use super::commands::CommandAction;
#[cfg(test)]
use super::composer::composer_widget;
use super::composer::{
    cursor_position, desired_height_for_width, render_lines as composer_render_lines, ComposerLine,
    ComposerRenderOptions, ComposerSnapshot, OverflowIndicator,
};
use super::dirty::{RedrawLevel, Region};
use super::event_loop::{
    enter_key_intent, EnterKeyIntent, EventLoop, InputEvent, TerminalSignal, UiAction, UiEvent,
};
use super::external_clipboard::{terminal_clipboard_sequence, ClipboardSink, SystemClipboard};
use super::external_editor::{EditorResult, ExternalEditorRunner, SystemExternalEditor};
use super::glyphs::user_line_prefix;
use super::metrics;
use super::patch_approval::{self, PatchApprovalModal, PatchPreview};
#[cfg(test)]
use super::status::status_widget;
use super::status::{status_line_text, StatusSnapshot, TokenUsageSnapshot, TurnStatus};
use super::terminal::{self, PendingSignal, TerminalSession};
use super::theme::{Theme, ThemeChoice};
#[cfg(test)]
use super::transcript::transcript_items_widget;
use super::transcript::{TranscriptItem, TranscriptState, TOOL_CALL_MAX_LINES};
use super::tui_decider::{PermissionChannels, PermissionReply, TuiDecider};
use super::visual_canvas::{
    BlockCursor, CanvasComposerSnapshot, CanvasLine, CanvasSpan, CanvasStatusSnapshot, FocusOwner,
    TextRole, VisualBlock, VisualBlockRole, VisualCanvasFrame, VisualCanvasSnapshot,
    VisualCanvasState,
};
use crate::bundled_extensions::{
    bundled_descriptor_by_id, bundled_extension_by_id, bundled_round_observer, ObserveOptions,
};
use crate::extension_enablement::{resolve_session_extensions, ExtensionSelection};
use crate::model_preference;
use anyhow::{anyhow, Result};
use crossterm::event::{self, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use euler_core::permissions::PermissionRequest;
use euler_core::{
    fold_session, heuristic_projection, read_resume_prefix, resume_session_from_folded_prefix,
    AgentResult, AgentTask, ApprovalMode, EulerHome, ModelTarget, ProvenanceWriter,
    ReasoningEffort, Session, SessionStore,
};
use euler_event::{EventEnvelope, EventKind};
use euler_provider::catalog::MergedModelCatalog;
use euler_sdk::{Capability, Extension};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::text::Line;
#[cfg(test)]
use ratatui::widgets::Paragraph;
#[cfg(test)]
use ratatui::Frame;
use std::collections::VecDeque;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(50);
const QUIT_ARM_WINDOW: Duration = Duration::from_secs(2);
const MIN_WORKED_DURATION: Duration = Duration::from_secs(5);
const QUIT_ARM_NOTICE: &str = "press Ctrl+C again to quit";

type CrosstermTerminal = terminal::InlineTerminal<CrosstermBackend<terminal::FrameBufferedStdout>>;

fn text_entry_modifiers(modifiers: KeyModifiers) -> bool {
    modifiers.is_empty()
        || modifiers == KeyModifiers::SHIFT
        || modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT)
        || modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)
}

fn is_slash_command_key(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('/')
        && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
}

#[cfg(test)]
mod chrome;
#[cfg(test)]
mod render_tests_support;
mod support;
mod visual;

#[cfg(test)]
use self::visual::{ratatui_lines_to_canvas, render_finalized_visual_items};

use self::support::{
    command_context, is_copy_key, merge_effects, read_terminal_event, session_root_status_path,
    update_token_usage,
};

pub struct App {
    terminal: CrosstermTerminal,
    _terminal_session: TerminalSession,
    event_loop: EventLoop,
    core: AppCore,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AppOptions {
    pub linefeed_history_insert: bool,
    pub theme_choice: ThemeChoice,
    pub theme_preference_path: Option<PathBuf>,
    pub model_catalog: Option<MergedModelCatalog>,
    pub session_store: Option<SessionStore>,
    pub extensions: ExtensionSelection,
    pub observe: ObserveOptions,
}

pub struct AppCore {
    state: AppState,
    permission_rx: Receiver<PermissionRequest>,
    reply_tx: Sender<PermissionReply>,
    bottom: BottomSurface,
    status: StatusSnapshot,
    model_catalog: MergedModelCatalog,
    session_store: Option<SessionStore>,
    active_session_home_managed: bool,
    token_usage: TokenUsageSnapshot,
    transcript: TranscriptState,
    visual_canvas: VisualCanvasState,
    visual_scroll_offset: usize,
    composer_navigation_width: u16,
    last_working_elapsed_secs: Option<u64>,
    modal: Option<Modal>,
    quit_armed: Option<Instant>,
    notice: Option<String>,
    pending_terminal_clipboard: Option<String>,
    interrupted_guidance: bool,
    in_flight_error: Option<String>,
    tool_artifacts_expanded: bool,
    theme: Theme,
    theme_choice: ThemeChoice,
    theme_preference_path: Option<PathBuf>,
    editor: Box<dyn ExternalEditorRunner>,
    clipboard: Box<dyn ClipboardSink>,
    pending_runs: VecDeque<PendingRunRequest>,
    in_flight_label: Option<String>,
    in_flight_cancellable: bool,
    extensions: ExtensionSelection,
    observe: ObserveOptions,
}

enum AppState {
    Empty,
    Idle {
        session: Box<Session<TuiDecider>>,
    },
    TurnInFlight {
        worker_rx: Receiver<TurnEvent>,
        interrupt_flag: Arc<AtomicBool>,
        started_at: Instant,
    },
}

enum TurnEvent {
    Event(EventEnvelope),
    TurnDone {
        outcome: TurnOutcome,
        session: Box<Session<TuiDecider>>,
    },
    ExtensionDone {
        request: ExtensionRunRequest,
        outcome: ExtensionOutcome,
        events: Vec<EventEnvelope>,
        session: Box<Session<TuiDecider>>,
    },
    CompanionDone {
        request: CompanionRunRequest,
        outcome: CompanionOutcome,
        events: Vec<EventEnvelope>,
        session: Box<Session<TuiDecider>>,
    },
}

struct TuiResume {
    session: Session<TuiDecider>,
    channels: PermissionChannels,
    events: Vec<EventEnvelope>,
    active_target: ModelTarget,
    display_label: String,
    recovery_closure_appended: bool,
    warning_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TurnOutcome {
    Complete,
    Cancelled,
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ExtensionOutcome {
    Complete(serde_json::Value),
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CompanionOutcome {
    Complete(AgentResult),
    Failed(String),
}

#[derive(Clone)]
struct ExtensionRunRequest {
    id: String,
    command: String,
    input: serde_json::Value,
    extension: &'static dyn Extension,
    capabilities: Vec<Capability>,
}

#[derive(Clone, Debug)]
struct CompanionRunRequest {
    task: AgentTask,
}

#[derive(Clone)]
enum PendingRunRequest {
    Extension(ExtensionRunRequest),
    Companion(CompanionRunRequest),
}

impl ExtensionRunRequest {
    fn label(&self) -> String {
        format!("extension {}.{}", self.id, self.command)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Modal {
    Permission(PermissionRequest),
    PatchApproval(PatchApprovalModal),
    Help,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoreEffect {
    None,
    Render,
    ThemeChanged,
    TerminalClipboard,
    ReplayHistory,
    ReplayHistoryWithScrollbackPurge,
    Quit,
}

impl App {
    pub fn enter_with_options(
        session: Session<TuiDecider>,
        channels: PermissionChannels,
        options: AppOptions,
    ) -> Result<Self> {
        let terminal_session = TerminalSession::enter()?;
        let mut terminal = terminal_session.ratatui_terminal()?;
        terminal.set_linefeed_history_insert_enabled(options.linefeed_history_insert);
        let event_loop = EventLoop::new(Instant::now());
        let core = AppCore::new_with_options(session, channels, options);
        set_terminal_theme_colors(&mut terminal, &core)?;
        Ok(Self {
            terminal,
            _terminal_session: terminal_session,
            event_loop,
            core,
        })
    }

    pub fn run(&mut self) -> Result<()> {
        self.request_render(RedrawLevel::Full);
        loop {
            self.poll_background();
            let timeout = self.poll_timeout();
            self.poll_terminal(timeout)?;
            if self.drain_actions()? {
                return Ok(());
            }
        }
    }

    fn poll_background(&mut self) {
        if let Some(signal) = terminal::take_pending_signal() {
            self.event_loop.push(UiEvent::Signal(match signal {
                PendingSignal::Interrupt => TerminalSignal::Interrupt,
                PendingSignal::Terminate => TerminalSignal::Terminate,
            }));
        }
        if self.core.drain_background() || self.core.mark_working_timer_dirty() {
            self.request_render(RedrawLevel::Full);
        }
    }

    fn poll_timeout(&self) -> Duration {
        self.event_loop
            .poll_timeout(Instant::now())
            .min(WORKER_POLL_INTERVAL)
    }

    fn poll_terminal(&mut self, timeout: Duration) -> Result<()> {
        // Drain every already-delivered event in one pass (bounded so a
        // hostile stream cannot starve rendering). A resize drag delivers
        // bursts faster than one replay; draining lets the event loop
        // coalesce them into a single Resize action instead of one
        // purge+replay per delivered event.
        const DRAIN_BUDGET: usize = 128;
        let mut saw_resize_event = false;
        let mut wait = timeout;
        for _ in 0..DRAIN_BUDGET {
            if !event::poll(wait)? {
                break;
            }
            wait = Duration::ZERO;
            if let Some(event) = read_terminal_event()? {
                match event {
                    UiEvent::Resize { width, height } => {
                        saw_resize_event = true;
                        metrics::record(metrics::Metric::ResizeEvent);
                        if self.core.turn_in_flight() {
                            self.terminal.suspend_linefeed_history_insert_after_resize();
                        }
                        self.terminal.note_resize_event(width, height);
                        self.event_loop.push(UiEvent::Resize { width, height });
                    }
                    event => self.event_loop.push(event),
                }
            }
        }
        if !saw_resize_event {
            if let Some(size) = self.terminal.observed_size_change()? {
                metrics::record(metrics::Metric::ResizeEvent);
                if self.core.turn_in_flight() {
                    self.terminal.suspend_linefeed_history_insert_after_resize();
                }
                self.event_loop.push(UiEvent::Resize {
                    width: size.width,
                    height: size.height,
                });
            }
        }
        Ok(())
    }

    fn drain_actions(&mut self) -> Result<bool> {
        let actions = self.event_loop.drain_ready(Instant::now());
        for action in actions {
            if self.handle_action(action)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn handle_action(&mut self, action: UiAction) -> Result<bool> {
        let effect = match action {
            UiAction::InputBatch(inputs) => self.handle_input_batch(inputs),
            UiAction::InterruptCurrentTurn => self.core.handle_terminal_interrupt(),
            UiAction::Shutdown => return self.shutdown(),
            UiAction::Resize { .. } => {
                metrics::record(metrics::Metric::ResizeAction);
                CoreEffect::ReplayHistoryWithScrollbackPurge
            }
            UiAction::Render(_) => {
                self.render_frame()?;
                return Ok(false);
            }
        };
        self.apply_effect(effect)
    }

    fn apply_effect(&mut self, effect: CoreEffect) -> Result<bool> {
        self.core.discard_terminal_clipboard_if_shadowed(effect);
        self.sync_terminal_theme_colors()?;
        match effect {
            CoreEffect::None => Ok(false),
            CoreEffect::Render => {
                self.request_render(RedrawLevel::Partial);
                Ok(false)
            }
            CoreEffect::ThemeChanged => {
                // Native scrollback already contains styled cells, so a theme
                // switch must rebuild history instead of only redrawing the
                // active viewport.
                self.replay_history(true)?;
                Ok(false)
            }
            CoreEffect::TerminalClipboard => {
                if let Some(sequence) = self.core.pending_terminal_clipboard.take() {
                    if let Err(error) = self.terminal.write_terminal_sequence(&sequence) {
                        self.core.notice =
                            Some(format!("copy failed: terminal clipboard failed: {error}"));
                    } else {
                        self.core.notice = Some("copied last assistant response".to_owned());
                    }
                } else {
                    self.core.notice =
                        Some("copy failed: terminal clipboard payload missing".to_owned());
                }
                self.request_render(RedrawLevel::Partial);
                Ok(false)
            }
            CoreEffect::ReplayHistory => {
                self.replay_history(false)?;
                Ok(false)
            }
            CoreEffect::ReplayHistoryWithScrollbackPurge => {
                self.replay_history(true)?;
                Ok(false)
            }
            CoreEffect::Quit => self.shutdown(),
        }
    }

    fn handle_input_batch(&mut self, inputs: Vec<InputEvent>) -> CoreEffect {
        let mut effect = CoreEffect::None;
        for input in inputs {
            effect = merge_effects(effect, self.core.handle_input(input));
            if effect == CoreEffect::Quit {
                break;
            }
        }
        effect
    }

    fn shutdown(&mut self) -> Result<bool> {
        if self.core.turn_in_flight() {
            self.core.deny_open_modal();
            // run_turn is synchronous. Hard-exit instead of joining an
            // in-flight provider call that may not return.
            terminal::restore_terminal();
            std::process::exit(0);
        }
        Ok(true)
    }

    fn request_render(&mut self, level: RedrawLevel) {
        for region in Region::ALL {
            self.event_loop
                .push(UiEvent::RenderRequested(region, level));
        }
    }

    fn render_frame(&mut self) -> Result<()> {
        metrics::record(metrics::Metric::RenderFrame);
        let width = self.terminal.active_width()?;
        let visual_canvas_frame = self.core.render_visual_canvas(width);
        self.terminal
            .set_review_scroll_offset(self.core.visual_scroll_offset());
        self.terminal.draw_visual_frame(&visual_canvas_frame)?;
        Ok(())
    }

    fn replay_history(&mut self, purge_scrollback: bool) -> Result<()> {
        // A replay clears and rewrites the whole canvas. Guard it with DEC
        // 2026 synchronized updates so supporting terminals paint one atomic
        // frame instead of a visible blank-then-refill sweep; the guard must
        // close even when the replay fails.
        self.terminal.begin_synchronized_update()?;
        let replay = self
            .terminal
            .reset_for_history_replay(purge_scrollback)
            .map_err(anyhow::Error::from)
            .and_then(|()| self.render_frame());
        let guard_closed = self.terminal.end_synchronized_update();
        if replay.is_err() {
            self.terminal.invalidate_cursor_position_authority();
        }
        replay?;
        guard_closed?;
        Ok(())
    }

    fn sync_terminal_theme_colors(&mut self) -> Result<()> {
        set_terminal_theme_colors(&mut self.terminal, &self.core)?;
        Ok(())
    }
}

fn set_terminal_theme_colors(terminal: &mut CrosstermTerminal, core: &AppCore) -> io::Result<()> {
    terminal.set_theme_colors(
        core.theme.palette.foreground,
        core.theme.palette.background,
        core.theme.palette.cursor,
    )
}

impl AppCore {
    #[cfg(test)]
    pub fn new(session: Session<TuiDecider>, channels: PermissionChannels) -> Self {
        Self::new_with_options(session, channels, AppOptions::default())
    }

    pub fn new_with_options(
        session: Session<TuiDecider>,
        channels: PermissionChannels,
        options: AppOptions,
    ) -> Self {
        let target = session.active_target().clone();
        let reasoning_effort = session.reasoning_effort();
        let session_id = session.session_id().to_owned();
        let cwd = session_root_status_path();
        let AppOptions {
            theme_choice,
            theme_preference_path,
            model_catalog,
            session_store,
            extensions,
            observe,
            ..
        } = options;
        let active_session_home_managed = session_store.is_some();
        let model_catalog = model_catalog.unwrap_or_else(|| {
            crate::model_catalog::load_model_catalog(
                crate::model_catalog::default_model_catalog_path().as_deref(),
            )
            .catalog
        });
        let theme = Theme::for_choice(theme_choice);
        let mut status = StatusSnapshot::new(target.provider.clone(), target.model.clone(), cwd);
        status.session_id = Some(session_id.clone());
        status.reasoning_effort = Some(reasoning_effort.as_str().to_owned());
        Self {
            state: AppState::Idle {
                session: Box::new(session),
            },
            permission_rx: channels.request_rx,
            reply_tx: channels.reply_tx,
            bottom: BottomSurface::new(command_context(
                &model_catalog,
                &target.provider,
                &target.model,
                reasoning_effort,
                theme_choice,
                Some(&session_id),
            )),
            status,
            model_catalog,
            session_store,
            active_session_home_managed,
            token_usage: TokenUsageSnapshot::default(),
            transcript: TranscriptState::default(),
            visual_canvas: VisualCanvasState::new(vec![TranscriptItem::Banner]),
            visual_scroll_offset: 0,
            composer_navigation_width: 80,
            last_working_elapsed_secs: None,
            modal: None,
            quit_armed: None,
            notice: None,
            pending_terminal_clipboard: None,
            interrupted_guidance: false,
            in_flight_error: None,
            tool_artifacts_expanded: false,
            theme,
            theme_choice,
            theme_preference_path,
            editor: Box::<SystemExternalEditor>::default(),
            clipboard: Box::<SystemClipboard>::default(),
            pending_runs: VecDeque::new(),
            in_flight_label: None,
            in_flight_cancellable: false,
            extensions,
            observe,
        }
    }

    fn rebuild_bottom_surface(&mut self) {
        let current_session_id = self.status.session_id.as_deref();
        self.bottom.reset_context(command_context(
            &self.model_catalog,
            &self.status.provider,
            &self.status.model,
            self.current_reasoning_effort(),
            self.theme_choice,
            current_session_id,
        ));
    }

    fn replace_bottom_surface_for_session(&mut self) {
        let current_session_id = self.status.session_id.as_deref();
        self.bottom = BottomSurface::new(command_context(
            &self.model_catalog,
            &self.status.provider,
            &self.status.model,
            self.current_reasoning_effort(),
            self.theme_choice,
            current_session_id,
        ));
    }

    fn current_reasoning_effort(&self) -> ReasoningEffort {
        self.status
            .reasoning_effort
            .as_deref()
            .and_then(ReasoningEffort::parse)
            .unwrap_or_default()
    }

    pub fn handle_input(&mut self, input: InputEvent) -> CoreEffect {
        if matches!(self.modal, Some(Modal::Help)) {
            return self.handle_help_input(input);
        }
        if self.modal.is_some() {
            return self.handle_modal_input(input);
        }
        match input {
            InputEvent::Paste(text) => self.handle_paste(&text),
            InputEvent::Mouse(mouse) => self.handle_mouse(mouse),
            InputEvent::Key(key) => self.handle_key(key),
        }
    }

    pub fn handle_interrupt(&mut self) -> CoreEffect {
        match &self.state {
            AppState::TurnInFlight { interrupt_flag, .. } => {
                if self.is_in_flight_cancellable() {
                    interrupt_flag.store(true, Ordering::SeqCst);
                    self.interrupted_guidance = true;
                } else {
                    // The interrupt is dropped, not deferred: extension
                    // commands do not observe the flag yet. Say so.
                    self.notice = Some(
                        "extension command is not cancellable; it will run to completion"
                            .to_owned(),
                    );
                }
                CoreEffect::Render
            }
            AppState::Idle { .. } => CoreEffect::None,
            AppState::Empty => CoreEffect::None,
        }
    }

    pub fn handle_terminal_interrupt(&mut self) -> CoreEffect {
        if !self.turn_in_flight() {
            return self.handle_ctrl_c();
        }
        if self.interrupted_guidance
            && self
                .quit_armed
                .is_some_and(|armed| Instant::now().duration_since(armed) <= QUIT_ARM_WINDOW)
        {
            return CoreEffect::Quit;
        }
        self.quit_armed = Some(Instant::now());
        self.handle_interrupt()
    }

    pub fn drain_background(&mut self) -> bool {
        let mut changed = self.drain_permissions();
        while let Some(event) = self.next_turn_event() {
            changed = true;
            self.handle_turn_event(event);
        }
        changed
    }

    pub fn turn_in_flight(&self) -> bool {
        matches!(self.state, AppState::TurnInFlight { .. })
    }

    fn is_in_flight_cancellable(&self) -> bool {
        self.in_flight_label.is_none() || self.in_flight_cancellable
    }

    fn handle_key(&mut self, key: KeyEvent) -> CoreEffect {
        if self.modal.is_some() {
            return CoreEffect::None;
        }
        if is_artifact_toggle_key(&key) {
            return self.toggle_tool_artifact_expansion();
        }
        if let Some(effect) = self.handle_visual_scroll_key(&key) {
            return effect;
        }
        if self.turn_in_flight() {
            return self.handle_key_in_flight(key);
        }
        if !matches!(self.bottom.owner(), BottomOwner::Composer) {
            return self.handle_surface_key(key);
        }
        self.handle_composer_key(key)
    }

    fn handle_key_in_flight(&mut self, key: KeyEvent) -> CoreEffect {
        match key.code {
            KeyCode::Char('c') | KeyCode::Char('C') if is_copy_key(&key) => {
                self.copy_last_assistant_response()
            }
            KeyCode::Char('x') | KeyCode::Char('X')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.notice = Some("external editor waits for the active turn".to_owned());
                CoreEffect::Render
            }
            KeyCode::Esc => self.handle_interrupt(),
            KeyCode::Char('c') | KeyCode::Char('C')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.handle_terminal_interrupt()
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.bottom.composer().submit_text().is_empty() {
                    CoreEffect::Quit
                } else {
                    CoreEffect::None
                }
            }
            _ if !matches!(self.bottom.owner(), BottomOwner::Composer) => {
                self.handle_surface_key(key)
            }
            KeyCode::Enter if enter_key_intent(&key) == Some(EnterKeyIntent::InsertNewline) => {
                self.edit_composer_text(|draft| draft.insert_newline())
            }
            KeyCode::Enter => self.turn_already_in_progress_notice(),
            _ if is_slash_command_key(&key) && self.bottom.composer().submit_text().is_empty() => {
                self.bottom.open_palette();
                CoreEffect::Render
            }
            KeyCode::Char(ch) if text_entry_modifiers(key.modifiers) => {
                self.edit_composer_text(|draft| draft.insert_char(ch))
            }
            KeyCode::Backspace => self.edit_composer_text(|draft| draft.backspace()),
            KeyCode::Delete => self.edit_composer_text(|draft| draft.delete()),
            KeyCode::Left => self.move_composer_cursor(|draft| draft.move_left()),
            KeyCode::Right => self.move_composer_cursor(|draft| draft.move_right()),
            KeyCode::Up => self.move_composer_up_or_history(),
            KeyCode::Down => self.move_composer_down_or_history(),
            KeyCode::Home => self.move_composer_cursor(|draft| draft.move_home()),
            KeyCode::End => self.move_composer_cursor(|draft| draft.move_end()),
            _ => CoreEffect::None,
        }
    }

    fn handle_visual_scroll_key(&mut self, key: &KeyEvent) -> Option<CoreEffect> {
        match key.code {
            KeyCode::PageUp => Some(self.scroll_visual_canvas_up(8)),
            KeyCode::PageDown => Some(self.scroll_visual_canvas_down(8)),
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(self.scroll_visual_canvas_up(1))
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(self.scroll_visual_canvas_down(1))
            }
            _ => None,
        }
    }

    fn scroll_visual_canvas_up(&mut self, rows: usize) -> CoreEffect {
        self.visual_scroll_offset = self.visual_scroll_offset.saturating_add(rows);
        CoreEffect::Render
    }

    fn scroll_visual_canvas_down(&mut self, rows: usize) -> CoreEffect {
        self.visual_scroll_offset = self.visual_scroll_offset.saturating_sub(rows);
        CoreEffect::Render
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> CoreEffect {
        if self.modal.is_some() {
            return CoreEffect::None;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_visual_canvas_up(3),
            MouseEventKind::ScrollDown => self.scroll_visual_canvas_down(3),
            _ => CoreEffect::None,
        }
    }

    fn handle_surface_key(&mut self, key: KeyEvent) -> CoreEffect {
        match key.code {
            KeyCode::Esc => {
                let event = self.bottom.cancel();
                self.surface_event(event)
            }
            KeyCode::Enter => {
                let event = self.bottom.confirm();
                self.surface_event(event)
            }
            KeyCode::Tab => {
                self.bottom.autocomplete();
                CoreEffect::Render
            }
            KeyCode::Down => {
                self.bottom.move_selection_down();
                CoreEffect::Render
            }
            KeyCode::Up => {
                self.bottom.move_selection_up();
                CoreEffect::Render
            }
            KeyCode::Char(ch) => {
                self.bottom.palette_insert(&ch.to_string());
                CoreEffect::Render
            }
            KeyCode::Backspace => self.edit_palette(BottomSurface::palette_backspace),
            KeyCode::Delete => self.edit_palette(BottomSurface::palette_delete),
            KeyCode::Left => self.edit_palette(BottomSurface::palette_move_left),
            KeyCode::Right => self.edit_palette(BottomSurface::palette_move_right),
            KeyCode::Home => self.edit_palette(BottomSurface::palette_move_home),
            KeyCode::End => self.edit_palette(BottomSurface::palette_move_end),
            _ => CoreEffect::None,
        }
    }

    fn handle_composer_key(&mut self, key: KeyEvent) -> CoreEffect {
        if let Some(effect) = self.handle_control_key(key) {
            return effect;
        }
        self.disarm_quit_notice();
        if key.code == KeyCode::Char('?')
            && key.modifiers.is_empty()
            && self.bottom.composer().submit_text().is_empty()
        {
            self.modal = Some(Modal::Help);
            return CoreEffect::Render;
        }
        if let Some(intent) = enter_key_intent(&key) {
            return self.handle_enter(intent);
        }
        match key.code {
            _ if is_slash_command_key(&key) && self.bottom.composer().submit_text().is_empty() => {
                self.bottom.open_palette();
                CoreEffect::Render
            }
            KeyCode::Char(ch) => self.edit_composer_text(|draft| draft.insert_char(ch)),
            KeyCode::Backspace => self.edit_composer_text(|draft| draft.backspace()),
            KeyCode::Delete => self.edit_composer_text(|draft| draft.delete()),
            KeyCode::Left => self.move_composer_cursor(|draft| draft.move_left()),
            KeyCode::Right => self.move_composer_cursor(|draft| draft.move_right()),
            KeyCode::Up => self.move_composer_up_or_history(),
            KeyCode::Down => self.move_composer_down_or_history(),
            KeyCode::Home => self.move_composer_cursor(|draft| draft.move_home()),
            KeyCode::End => self.move_composer_cursor(|draft| draft.move_end()),
            _ => CoreEffect::None,
        }
    }

    fn handle_control_key(&mut self, key: KeyEvent) -> Option<CoreEffect> {
        match key.code {
            KeyCode::Char('c') | KeyCode::Char('C') if is_copy_key(&key) => {
                Some(self.copy_last_assistant_response())
            }
            KeyCode::Char('c') | KeyCode::Char('C')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                Some(self.handle_ctrl_c())
            }
            KeyCode::Char('x') | KeyCode::Char('X')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                Some(self.open_external_editor())
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => self
                .bottom
                .composer()
                .submit_text()
                .is_empty()
                .then_some(CoreEffect::Quit),
            KeyCode::Esc => Some(CoreEffect::None),
            _ => None,
        }
    }

    fn handle_enter(&mut self, intent: EnterKeyIntent) -> CoreEffect {
        match intent {
            EnterKeyIntent::InsertNewline => {
                self.edit_composer_text(|draft| draft.insert_newline())
            }
            EnterKeyIntent::Submit => self.handle_submit(),
        }
    }

    fn handle_paste(&mut self, text: &str) -> CoreEffect {
        if self.modal.is_some() {
            return CoreEffect::None;
        }
        if !matches!(self.bottom.owner(), BottomOwner::Composer) {
            return CoreEffect::None;
        }
        self.bottom.edit_composer(|draft| {
            let _ = draft.insert_bracketed_paste(text);
        });
        CoreEffect::Render
    }

    fn handle_modal_input(&mut self, input: InputEvent) -> CoreEffect {
        let InputEvent::Key(key) = input else {
            return CoreEffect::None;
        };
        if matches!(self.modal, Some(Modal::PatchApproval(_))) {
            return self.handle_patch_modal_key(key);
        }
        match key.code {
            KeyCode::Char('1') | KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.reply_to_modal(PermissionReply::Allow)
            }
            KeyCode::Char('3') | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.reply_to_modal(PermissionReply::Deny)
            }
            _ if modal_quit_key(&key) => {
                self.reply_to_modal(PermissionReply::Deny);
                CoreEffect::Quit
            }
            KeyCode::Char('2') | KeyCode::Char('a') | KeyCode::Char('A') => {
                self.reply_to_modal(PermissionReply::AllowAll)
            }
            _ => CoreEffect::None,
        }
    }

    fn handle_help_input(&mut self, input: InputEvent) -> CoreEffect {
        let InputEvent::Key(key) = input else {
            return CoreEffect::None;
        };
        self.modal = None;
        match key.code {
            KeyCode::Char('c') | KeyCode::Char('C')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.handle_ctrl_c()
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => CoreEffect::Quit,
            KeyCode::Esc => CoreEffect::Render,
            _ => CoreEffect::Render,
        }
    }

    fn handle_patch_modal_key(&mut self, key: KeyEvent) -> CoreEffect {
        match key.code {
            KeyCode::Char('1') | KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.reply_to_modal(PermissionReply::Allow)
            }
            KeyCode::Char('3') | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.reply_to_modal(PermissionReply::Deny)
            }
            KeyCode::Char('r') | KeyCode::Char('R') => self.expand_patch_modal(),
            _ if modal_quit_key(&key) => {
                self.reply_to_modal(PermissionReply::Deny);
                CoreEffect::Quit
            }
            KeyCode::Char('2') | KeyCode::Char('a') | KeyCode::Char('A') => {
                self.reply_to_modal(PermissionReply::AllowAll)
            }
            _ => CoreEffect::None,
        }
    }

    fn expand_patch_modal(&mut self) -> CoreEffect {
        let Some(Modal::PatchApproval(modal)) = &mut self.modal else {
            return CoreEffect::None;
        };
        modal.expanded = true;
        CoreEffect::Render
    }

    fn handle_submit(&mut self) -> CoreEffect {
        let prompt = self.bottom.composer().submit_text();
        if prompt.trim().is_empty() {
            return CoreEffect::None;
        }
        let AppState::Idle { .. } = self.state else {
            return self.turn_already_in_progress_notice();
        };
        self.visual_scroll_offset = 0;
        self.bottom.record_submission(&prompt);
        let session = self.take_idle_session();
        self.rebuild_bottom_surface();
        self.spawn_turn(prompt, session);
        CoreEffect::Render
    }

    fn take_idle_session(&mut self) -> Box<Session<TuiDecider>> {
        match std::mem::replace(&mut self.state, AppState::Empty) {
            AppState::Idle { session } => session,
            state => {
                self.state = state;
                unreachable!("submit checked idle state")
            }
        }
    }

    fn spawn_turn(&mut self, prompt: String, mut session: Box<Session<TuiDecider>>) {
        let (worker_tx, worker_rx) = mpsc::channel();
        let interrupt_flag = Arc::new(AtomicBool::new(false));
        let worker_interrupt = Arc::clone(&interrupt_flag);
        std::thread::spawn(move || {
            let stream_tx = worker_tx.clone();
            let result =
                session.run_turn_with_sink(&prompt, Arc::clone(&worker_interrupt), move |event| {
                    let _ = stream_tx.send(TurnEvent::Event(event.clone()));
                });
            let outcome = match result {
                Ok(_) => TurnOutcome::Complete,
                Err(euler_core::SessionError::Cancelled) => TurnOutcome::Cancelled,
                Err(error) => TurnOutcome::Failed(error.to_string()),
            };
            let _ = worker_tx.send(TurnEvent::TurnDone { outcome, session });
        });
        self.state = AppState::TurnInFlight {
            worker_rx,
            interrupt_flag,
            started_at: Instant::now(),
        };
        self.in_flight_label = Some("turn".to_owned());
        self.in_flight_cancellable = true;
        self.last_working_elapsed_secs = None;
        self.interrupted_guidance = false;
        self.in_flight_error = None;
    }

    fn surface_event(&mut self, event: SurfaceEvent) -> CoreEffect {
        match event {
            SurfaceEvent::None => CoreEffect::Render,
            SurfaceEvent::Message(message) => self.notice_item(message),
            SurfaceEvent::Action(action) => self.handle_command_action(action),
        }
    }

    fn handle_command_action(&mut self, action: CommandAction) -> CoreEffect {
        match action {
            CommandAction::NewSession => self.start_new_session(),
            CommandAction::Quit => CoreEffect::Quit,
            CommandAction::SwitchModel { provider, model } => self.switch_model(provider, model),
            CommandAction::SetReasoningEffort { effort } => self.set_reasoning_effort(effort),
            CommandAction::CompactSession => self.compact_session(),
            CommandAction::ExportSession { path } => self.export_session(path),
            CommandAction::ExtensionRun { id, command, input } => {
                self.extension_run(id, command, input)
            }
            CommandAction::CompanionRun { input } => self.companion_run(input),
            CommandAction::ShowStatus => self.show_status(),
            CommandAction::Login { provider } => self.login_guidance(provider),
            CommandAction::Logout { provider } => self.logout_guidance(provider),
            CommandAction::SetTheme { choice } => self.set_theme(choice),
            CommandAction::SetPermissionMode { capability, mode } => {
                self.set_permission_mode(capability, mode)
            }
            CommandAction::ShowHelp { text } => self.summary_item(text),
            CommandAction::ResumeSession { session_id } => {
                self.resume_session_from_picker(session_id)
            }
            CommandAction::ScrollViewportToBottom => {
                self.transcript.scroll_to_bottom();
                self.visual_scroll_offset = 0;
                CoreEffect::Render
            }
            CommandAction::CopyLastAssistantResponse => self.copy_last_assistant_response(),
            CommandAction::NameSession { name } => self.name_current_session(name),
        }
    }

    fn start_new_session(&mut self) -> CoreEffect {
        if self.turn_in_flight() {
            return self.notice_item("new session waits for the active turn".to_owned());
        }
        let AppState::Idle { .. } = self.state else {
            return self.notice_item("new session needs an active session".to_owned());
        };
        let created = self.session_store().and_then(|store| {
            let record = store.create_session()?;
            Ok((record.id().to_owned(), record.events_path().to_path_buf()))
        });
        let (session_id, events_path) = match created {
            Ok(created) => created,
            Err(error) => return self.notice_item(format!("new session failed: {error}")),
        };
        let writer = match ProvenanceWriter::new(&events_path) {
            Ok(writer) => writer,
            Err(error) => return self.notice_item(format!("new session failed: {error}")),
        };
        let old_session = self.take_idle_session();
        let active_target = old_session.active_target().clone();
        let reasoning_effort = old_session.reasoning_effort();
        let (decider, channels) = TuiDecider::new();
        let session = old_session
            .into_fresh_session(session_id.clone(), decider)
            .with_provenance(writer);
        let events = session.events().to_vec();

        self.permission_rx = channels.request_rx;
        self.reply_tx = channels.reply_tx;
        self.state = AppState::Idle {
            session: Box::new(session),
        };
        self.status.provider = active_target.provider;
        self.status.model = active_target.model;
        self.status.session_id = Some(session_id.clone());
        self.status.reasoning_effort = Some(reasoning_effort.as_str().to_owned());
        self.active_session_home_managed = true;
        self.replace_bottom_surface_for_session();
        self.rebuild_transcript_from_events(&events);
        self.visual_scroll_offset = 0;
        self.token_usage = TokenUsageSnapshot::default();
        self.tool_artifacts_expanded = false;
        self.modal = None;
        self.quit_armed = None;
        self.last_working_elapsed_secs = None;
        self.interrupted_guidance = false;
        self.in_flight_error = None;
        self.notice = Some(format!("new session {session_id}"));
        CoreEffect::ReplayHistoryWithScrollbackPurge
    }

    fn set_reasoning_effort(&mut self, effort: ReasoningEffort) -> CoreEffect {
        let AppState::Idle { session } = &mut self.state else {
            return self.notice_item("reasoning effort waits for the active turn".to_owned());
        };
        match session.set_reasoning_effort(effort, "user") {
            Ok(true) => {
                self.status.reasoning_effort = Some(effort.as_str().to_owned());
                self.rebuild_bottom_surface();
                self.notice_item(format!("reasoning effort set to {}", effort.as_str()))
            }
            Ok(false) => self.notice_item(format!("reasoning effort already {}", effort.as_str())),
            Err(error) => self.notice_item(format!("reasoning effort rejected: {error}")),
        }
    }

    fn compact_session(&mut self) -> CoreEffect {
        let AppState::Idle { session } = &mut self.state else {
            return self.notice_item("compaction waits for the active turn".to_owned());
        };
        let start = session.events().len();
        let projection = heuristic_projection(session.events());
        if session.try_compact(&projection) {
            let new_events = session.events()[start..].to_vec();
            for event in new_events {
                self.transcript.push_event(event);
                self.queue_finalized_visual_output_for_latest_event();
            }
            return self.notice_item("compacted eligible history".to_owned());
        }
        self.notice_item("nothing eligible to compact".to_owned())
    }

    fn export_session(&mut self, path: Option<String>) -> CoreEffect {
        let (session_id, events) = match &self.state {
            AppState::Idle { session } => {
                (session.session_id().to_owned(), session.events().to_vec())
            }
            _ => return self.notice_item("export waits for the active turn".to_owned()),
        };
        let path = match path.map(PathBuf::from) {
            Some(path) => path,
            None => match self.default_export_path(&session_id) {
                Ok(path) => path,
                Err(error) => return self.notice_item(format!("export failed: {error}")),
            },
        };
        let payload = serde_json::json!({
            "session_id": session_id,
            "provider": self.status.provider,
            "model": self.status.model,
            "reasoning_effort": self.current_reasoning_effort().as_str(),
            "events": events,
        });
        match serde_json::to_vec_pretty(&payload)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| write_new_file(&path, &bytes).map_err(anyhow::Error::from))
        {
            Ok(()) => self.notice_item(format!("session exported to {}", path.display())),
            Err(error) => self.notice_item(format!("export failed: {error}")),
        }
    }

    fn extension_run(
        &mut self,
        id: String,
        command: String,
        input: serde_json::Value,
    ) -> CoreEffect {
        let request = match self.resolve_extension_run(id, command, input) {
            Ok(request) => request,
            Err(error) => return self.notice_item(format!("extension run failed: {error}")),
        };
        match std::mem::replace(&mut self.state, AppState::Empty) {
            AppState::Idle { session } => {
                self.spawn_extension_run(request, session);
                CoreEffect::Render
            }
            state @ AppState::TurnInFlight { .. } => {
                let label = request.label();
                self.state = state;
                self.pending_runs
                    .push_back(PendingRunRequest::Extension(request));
                self.notice = Some(format!("queued {label}"));
                CoreEffect::Render
            }
            AppState::Empty => {
                self.state = AppState::Empty;
                self.notice_item("extension run needs an active session".to_owned())
            }
        }
    }

    fn resolve_extension_run(
        &self,
        id: String,
        command: String,
        input: serde_json::Value,
    ) -> Result<ExtensionRunRequest> {
        let descriptor =
            bundled_descriptor_by_id(&id)?.ok_or_else(|| anyhow!("unknown extension id: {id}"))?;
        let command_descriptor = descriptor
            .command(&command)
            .ok_or_else(|| anyhow!("unknown command for extension {id}: {command}"))?;
        let bundled =
            bundled_extension_by_id(&id).ok_or_else(|| anyhow!("unknown extension id: {id}"))?;
        Ok(ExtensionRunRequest {
            id,
            command,
            input,
            extension: bundled.extension,
            capabilities: command_descriptor.required_capabilities.clone(),
        })
    }

    fn spawn_extension_run(
        &mut self,
        request: ExtensionRunRequest,
        mut session: Box<Session<TuiDecider>>,
    ) {
        let (worker_tx, worker_rx) = mpsc::channel();
        let worker_request = request.clone();
        let label = request.label();
        std::thread::spawn(move || {
            let start = session.events().len();
            let result = session.execute_extension_command(
                worker_request.extension,
                &worker_request.command,
                worker_request.input.clone(),
                worker_request.capabilities.iter().copied(),
            );
            let events = session.events()[start..].to_vec();
            let outcome = match result {
                Ok(output) => ExtensionOutcome::Complete(output),
                Err(error) => ExtensionOutcome::Failed(error.to_string()),
            };
            let _ = worker_tx.send(TurnEvent::ExtensionDone {
                request: worker_request,
                outcome,
                events,
                session,
            });
        });
        self.state = AppState::TurnInFlight {
            worker_rx,
            interrupt_flag: Arc::new(AtomicBool::new(false)),
            started_at: Instant::now(),
        };
        self.in_flight_label = Some(label);
        self.in_flight_cancellable = false;
        self.last_working_elapsed_secs = None;
        self.interrupted_guidance = false;
        self.in_flight_error = None;
    }

    fn companion_run(&mut self, input: serde_json::Value) -> CoreEffect {
        let request = match crate::companion_run::parse_agent_task_value(&input) {
            Ok(task) => CompanionRunRequest { task },
            Err(error) => return self.notice_item(format!("companion run failed: {error}")),
        };
        match std::mem::replace(&mut self.state, AppState::Empty) {
            AppState::Idle { session } => {
                self.spawn_companion_run(request, session);
                CoreEffect::Render
            }
            state @ AppState::TurnInFlight { .. } => {
                self.state = state;
                self.pending_runs
                    .push_back(PendingRunRequest::Companion(request));
                self.notice = Some("queued companion run".to_owned());
                CoreEffect::Render
            }
            AppState::Empty => {
                self.state = AppState::Empty;
                self.notice_item("companion run needs an active session".to_owned())
            }
        }
    }

    fn spawn_companion_run(
        &mut self,
        request: CompanionRunRequest,
        mut session: Box<Session<TuiDecider>>,
    ) {
        let (worker_tx, worker_rx) = mpsc::channel();
        let worker_request = request.clone();
        std::thread::spawn(move || {
            let start = session.events().len();
            let result = session.spawn_companion(worker_request.task.clone());
            let events = session.events()[start..].to_vec();
            let outcome = match result {
                Ok(summary) => CompanionOutcome::Complete(summary.result),
                Err(error) => CompanionOutcome::Failed(error.to_string()),
            };
            let _ = worker_tx.send(TurnEvent::CompanionDone {
                request: worker_request,
                outcome,
                events,
                session,
            });
        });
        self.state = AppState::TurnInFlight {
            worker_rx,
            interrupt_flag: Arc::new(AtomicBool::new(false)),
            started_at: Instant::now(),
        };
        self.in_flight_label = Some("companion run".to_owned());
        self.in_flight_cancellable = false;
        self.last_working_elapsed_secs = None;
        self.interrupted_guidance = false;
        self.in_flight_error = None;
    }

    fn show_status(&mut self) -> CoreEffect {
        let session = self.status.session_id.as_deref().unwrap_or("none");
        self.summary_item(format!(
            "session: {session}\nmodel: {}::{}\neffort: {}\ntheme: {} ({})",
            self.status.provider,
            self.status.model,
            self.current_reasoning_effort().as_str(),
            self.theme_choice.label(),
            self.theme_choice.as_str()
        ))
    }

    fn login_guidance(&mut self, provider: String) -> CoreEffect {
        self.summary_item(format!(
            "Run outside the TUI:\neuler login --provider {provider}\n\nThe picker stays offline; auth is checked when a request uses the provider."
        ))
    }

    fn logout_guidance(&mut self, provider: String) -> CoreEffect {
        self.summary_item(format!(
            "Run outside the TUI:\neuler logout --provider {provider}"
        ))
    }

    fn set_theme(&mut self, choice: ThemeChoice) -> CoreEffect {
        self.theme_choice = choice;
        self.theme = Theme::for_choice(choice);
        self.rebuild_bottom_surface();
        match self
            .theme_preference_path
            .as_deref()
            .map(|path| model_preference::save_theme_preference(path, choice.as_str()))
        {
            Some(Err(error)) => {
                self.push_notice_item(format!("theme set; preference not saved: {error}"));
            }
            _ => {
                self.push_notice_item(format!("theme set to {}", choice.as_str()));
            }
        }
        CoreEffect::ThemeChanged
    }

    fn name_current_session(&mut self, name: String) -> CoreEffect {
        let AppState::Idle { session } = &mut self.state else {
            return self.notice_item("session naming waits for the active turn".to_owned());
        };
        let session_id = session.session_id().to_owned();
        let result = session.rename_session(&name);
        match result {
            Ok(normalized) => {
                self.rebuild_bottom_surface();
                self.notice = match self.refresh_current_session_metadata(&session_id) {
                    Ok(()) => Some(format!("session named {normalized}")),
                    Err(error) => Some(format!(
                        "session named {normalized}; metadata refresh failed: {error}"
                    )),
                };
                CoreEffect::Render
            }
            Err(error) => self.notice_item(format!("session naming failed: {error}")),
        }
    }

    fn resume_session_from_picker(&mut self, session_id: String) -> CoreEffect {
        let current_session_id = match &self.state {
            AppState::Idle { session } => session.session_id().to_owned(),
            AppState::TurnInFlight { .. } => {
                return self.notice_item("resume waits for the active turn".to_owned());
            }
            AppState::Empty => {
                return self.notice_item("resume needs an active session".to_owned())
            }
        };
        if current_session_id == session_id {
            return self.notice_item(format!("already using session {session_id}"));
        }

        match self.build_tui_resume(&session_id) {
            Ok(resume) => self.accept_tui_resume(session_id, resume),
            Err(error) => self.notice_item(format!("resume failed: {error}")),
        }
    }

    fn build_tui_resume(&mut self, session_id: &str) -> Result<TuiResume> {
        let record = self
            .session_store()?
            .find_session(session_id)?
            .ok_or_else(|| anyhow!("no session found with id {session_id}"))?;
        let prefix = read_resume_prefix(record.events_path())?;
        let root = std::env::current_dir().unwrap_or_else(|_| session_root_status_path());
        let mut seed_config = crate::session_config(
            root.clone(),
            self.status.provider.clone(),
            self.status.model.clone(),
            session_id.to_owned(),
        );
        seed_config.extensions_enabled =
            resolve_session_extensions(&seed_config.root, &self.extensions)?;
        let observer = bundled_round_observer(&self.observe, &seed_config.extensions_enabled)?;
        if let Some((observer_config, _)) = &observer {
            seed_config.round_observer = Some(observer_config.clone());
        }
        let folded = fold_session(&seed_config, prefix)?;
        let original = folded
            .original_target
            .as_ref()
            .unwrap_or(&folded.active_target);
        let mut config = crate::session_config(
            root,
            original.provider.clone(),
            original.model.clone(),
            session_id.to_owned(),
        );
        config.extensions_enabled = seed_config.extensions_enabled;
        config.round_observer = seed_config.round_observer;
        let providers = crate::resume_provider_set(
            folded
                .original_target
                .as_ref()
                .unwrap_or(&folded.active_target),
            &folded.active_target,
            None,
        )?;
        let writer = ProvenanceWriter::new(record.events_path())?;
        let (decider, channels) = TuiDecider::new();
        let outcome =
            resume_session_from_folded_prefix(config, providers, decider, writer, folded)?;
        let mut session = outcome.session;
        if let Some((_, extension)) = observer {
            session.set_observer_extension(extension);
        }
        let events = session.events().to_vec();
        Ok(TuiResume {
            session,
            channels,
            events,
            active_target: outcome.active_target,
            display_label: record.display_label().to_owned(),
            recovery_closure_appended: outcome.recovery_closure_appended,
            warning_count: outcome.warnings.len(),
        })
    }

    fn accept_tui_resume(&mut self, session_id: String, resume: TuiResume) -> CoreEffect {
        let reasoning_effort = resume.session.reasoning_effort();
        self.permission_rx = resume.channels.request_rx;
        self.reply_tx = resume.channels.reply_tx;
        self.state = AppState::Idle {
            session: Box::new(resume.session),
        };
        self.status.provider = resume.active_target.provider.clone();
        self.status.model = resume.active_target.model.clone();
        self.status.session_id = Some(session_id.clone());
        self.status.reasoning_effort = Some(reasoning_effort.as_str().to_owned());
        self.active_session_home_managed = true;
        self.replace_bottom_surface_for_session();
        self.rebuild_transcript_from_events(&resume.events);
        self.visual_scroll_offset = 0;
        self.modal = None;
        self.quit_armed = None;
        self.last_working_elapsed_secs = None;
        self.interrupted_guidance = false;
        self.in_flight_error = None;
        let display = resume.display_label.clone();
        let mut notice = if display == session_id {
            format!("resumed session {session_id}")
        } else {
            format!("resumed session {display}")
        };
        if resume.recovery_closure_appended {
            notice.push_str("; recovery closure appended");
        }
        if resume.warning_count > 0 {
            notice.push_str(&format!("; {} warning(s)", resume.warning_count));
        }
        self.notice = Some(notice);
        CoreEffect::ReplayHistoryWithScrollbackPurge
    }

    fn rebuild_transcript_from_events(&mut self, events: &[EventEnvelope]) {
        let mut transcript = TranscriptState::default();
        let mut token_usage = TokenUsageSnapshot::default();
        for event in events {
            update_token_usage(&mut token_usage, event, self.active_context_window_tokens());
            transcript.push_event(event.clone());
        }
        transcript.scroll_to_bottom();
        let mut finalized = vec![TranscriptItem::Banner];
        finalized.extend(transcript.items());
        self.transcript = transcript;
        self.token_usage = token_usage;
        self.visual_canvas = VisualCanvasState::new(finalized);
    }

    fn switch_model(&mut self, provider: String, model: String) -> CoreEffect {
        let AppState::Idle { session } = &mut self.state else {
            return self.notice_item("model switch waits for the active turn".to_owned());
        };
        match session.switch_model(&provider, &model, "user") {
            Ok(true) => self.accept_model_switch(provider, model, true),
            Ok(false) => self.accept_model_switch(provider, model, false),
            Err(error) => self.notice_item(format!("model switch rejected: {error}")),
        }
    }

    fn accept_model_switch(
        &mut self,
        provider: String,
        model: String,
        switched: bool,
    ) -> CoreEffect {
        self.status.provider = provider.clone();
        self.status.model = model.clone();
        if switched {
            self.token_usage = TokenUsageSnapshot::default();
        }
        self.rebuild_bottom_surface();
        match model_preference::save_model_preference_to_default(&provider, &model) {
            Ok(()) => self.notice_item(format!("model set to {provider}/{model}")),
            Err(error) => self.notice_item(format!("model set; preference not saved: {error}")),
        }
    }

    fn set_permission_mode(&mut self, capability: Capability, mode: ApprovalMode) -> CoreEffect {
        let AppState::Idle { session } = &mut self.state else {
            return self.notice_item("permission mode waits for the active turn".to_owned());
        };
        session.set_permission_mode(capability, mode);
        self.notice_item(format!(
            "permission {} set to {:?}",
            capability.as_str(),
            mode
        ))
    }

    fn handle_ctrl_c(&mut self) -> CoreEffect {
        let now = Instant::now();
        if self
            .quit_armed
            .is_some_and(|armed| now.duration_since(armed) <= QUIT_ARM_WINDOW)
        {
            return CoreEffect::Quit;
        }
        self.quit_armed = Some(now);
        self.notice = Some(QUIT_ARM_NOTICE.to_owned());
        CoreEffect::Render
    }

    fn disarm_quit_notice(&mut self) {
        let was_armed = self.quit_armed.take().is_some();
        if was_armed && self.notice.as_deref() == Some(QUIT_ARM_NOTICE) {
            self.notice = None;
        }
    }

    fn edit_composer_text(
        &mut self,
        edit: impl FnOnce(&mut super::composer::ComposerDraft),
    ) -> CoreEffect {
        self.bottom.edit_composer(edit);
        CoreEffect::Render
    }

    fn move_composer_cursor(
        &mut self,
        edit: impl FnOnce(&mut super::composer::ComposerDraft),
    ) -> CoreEffect {
        self.bottom.move_composer_cursor(edit);
        CoreEffect::Render
    }

    fn move_composer_up_or_history(&mut self) -> CoreEffect {
        self.bottom
            .move_up_or_recall_history(self.composer_navigation_width);
        CoreEffect::Render
    }

    fn move_composer_down_or_history(&mut self) -> CoreEffect {
        self.bottom
            .move_down_or_recall_history(self.composer_navigation_width);
        CoreEffect::Render
    }

    fn turn_already_in_progress_notice(&mut self) -> CoreEffect {
        self.notice = Some("turn already in progress".to_owned());
        CoreEffect::Render
    }

    fn edit_palette(&mut self, edit: impl FnOnce(&mut BottomSurface)) -> CoreEffect {
        edit(&mut self.bottom);
        CoreEffect::Render
    }

    fn toggle_tool_artifact_expansion(&mut self) -> CoreEffect {
        if !matches!(self.bottom.owner(), BottomOwner::Composer) {
            return CoreEffect::None;
        }
        if !self
            .visual_canvas
            .has_foldable_shell_artifact(TOOL_CALL_MAX_LINES)
        {
            return CoreEffect::None;
        }
        self.tool_artifacts_expanded = !self.tool_artifacts_expanded;
        self.visual_canvas.invalidate_history_cache();
        self.visual_scroll_offset = 0;
        CoreEffect::ReplayHistoryWithScrollbackPurge
    }

    fn open_external_editor(&mut self) -> CoreEffect {
        let draft = self.bottom.composer().submit_text();
        match self.editor.edit(&draft) {
            EditorResult::Updated(contents) => {
                self.bottom.replace_composer_text(&contents);
                self.quit_armed = None;
                self.notice = Some("draft updated from editor".to_owned());
            }
            EditorResult::Unset => {
                self.notice = Some("EDITOR is not set; draft unchanged".to_owned());
            }
            EditorResult::Failed(message) => {
                self.notice = Some(format!("editor failed: {message}; draft unchanged"));
            }
        }
        CoreEffect::Render
    }

    fn copy_last_assistant_response(&mut self) -> CoreEffect {
        let Some(response) = self.transcript.last_visible_assistant_response() else {
            self.notice = Some("no assistant response to copy".to_owned());
            return CoreEffect::Render;
        };
        match self.clipboard.copy(&response) {
            Ok(()) => self.notice = Some("copied last assistant response".to_owned()),
            Err(message) => match terminal_clipboard_sequence(&response) {
                Ok(sequence) => {
                    self.pending_terminal_clipboard = Some(sequence);
                    self.notice = None;
                    return CoreEffect::TerminalClipboard;
                }
                Err(terminal_error) => {
                    self.notice = Some(format!("copy failed: {message}; {terminal_error}"));
                }
            },
        }
        CoreEffect::Render
    }

    fn discard_terminal_clipboard_if_shadowed(&mut self, effect: CoreEffect) {
        if effect != CoreEffect::TerminalClipboard {
            self.pending_terminal_clipboard = None;
        }
    }

    fn drain_permissions(&mut self) -> bool {
        let mut changed = false;
        while self.modal.is_none() {
            changed |= self.drain_turn_events();
            match self.permission_rx.try_recv() {
                Ok(request) => {
                    self.drain_turn_events();
                    self.modal = Some(self.modal_for_request(request));
                    changed = true;
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        changed
    }

    fn drain_turn_events(&mut self) -> bool {
        let mut changed = false;
        while let Some(event) = self.next_turn_event() {
            self.handle_turn_event(event);
            changed = true;
        }
        changed
    }

    fn modal_for_request(&self, request: PermissionRequest) -> Modal {
        if !patch_approval::is_patch_permission(&request) {
            return Modal::Permission(request);
        }
        Modal::PatchApproval(PatchApprovalModal {
            preview: patch_approval::preview_from_events(self.transcript.events()),
            request,
            expanded: false,
        })
    }

    fn next_turn_event(&mut self) -> Option<TurnEvent> {
        let AppState::TurnInFlight { worker_rx, .. } = &mut self.state else {
            return None;
        };
        worker_rx.try_recv().ok()
    }

    fn handle_turn_event(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::Event(event) => {
                let is_tool_call = event.kind.as_str() == EventKind::TOOL_CALL;
                self.record_in_flight_error(&event);
                self.update_token_usage_from_event(&event);
                self.transcript.push_event(event);
                self.queue_finalized_visual_output_for_latest_event();
                if is_tool_call {
                    self.refresh_patch_modal_preview();
                }
            }
            TurnEvent::TurnDone { outcome, session } => {
                let elapsed = self.working_elapsed();
                self.last_working_elapsed_secs = None;
                self.handle_turn_outcome(outcome, elapsed);
                self.accept_worker_session_or_continue(session);
            }
            TurnEvent::ExtensionDone {
                request,
                outcome,
                events,
                session,
            } => {
                let elapsed = self.working_elapsed();
                for event in events {
                    self.update_token_usage_from_event(&event);
                    self.transcript.push_event(event);
                    self.queue_finalized_visual_output_for_latest_event();
                }
                self.last_working_elapsed_secs = None;
                self.handle_extension_outcome(&request, outcome, elapsed);
                self.accept_worker_session_or_continue(session);
            }
            TurnEvent::CompanionDone {
                request,
                outcome,
                events,
                session,
            } => {
                let elapsed = self.working_elapsed();
                for event in events {
                    self.update_token_usage_from_event(&event);
                    self.transcript.push_event(event);
                    self.queue_finalized_visual_output_for_latest_event();
                }
                self.last_working_elapsed_secs = None;
                self.handle_companion_outcome(&request, outcome, elapsed);
                self.accept_worker_session_or_continue(session);
            }
        }
    }

    fn update_token_usage_from_event(&mut self, event: &EventEnvelope) {
        let context_window_tokens = self.active_context_window_tokens();
        update_token_usage(&mut self.token_usage, event, context_window_tokens);
    }

    fn active_context_window_tokens(&self) -> Option<u64> {
        self.model_catalog
            .provider(&self.status.provider)
            .and_then(|provider| {
                provider
                    .models()
                    .find(|model| model.id() == self.status.model)
            })
            .and_then(|model| model.context_window_tokens())
    }

    fn accept_worker_session_or_continue(&mut self, session: Box<Session<TuiDecider>>) {
        if self.active_session_home_managed {
            let session_id = session.session_id().to_owned();
            if let Err(error) = self.refresh_current_session_metadata(&session_id) {
                self.notice = Some(format!("session metadata refresh failed: {error}"));
            }
        }
        if let Some(request) = self.pending_runs.pop_front() {
            match request {
                PendingRunRequest::Extension(request) => self.spawn_extension_run(request, session),
                PendingRunRequest::Companion(request) => self.spawn_companion_run(request, session),
            }
        } else {
            self.state = AppState::Idle { session };
            self.in_flight_label = None;
            self.in_flight_cancellable = false;
        }
    }

    fn handle_extension_outcome(
        &mut self,
        request: &ExtensionRunRequest,
        outcome: ExtensionOutcome,
        elapsed: Option<Duration>,
    ) {
        if let Some(duration) = elapsed.filter(|duration| *duration >= MIN_WORKED_DURATION) {
            self.push_finalized_visual_item(TranscriptItem::WorkedDuration(format_live_elapsed(
                duration,
            )));
        }
        match outcome {
            ExtensionOutcome::Complete(output) => {
                let output = serde_json::to_string(&output).unwrap_or_else(|_| "null".to_owned());
                self.push_finalized_visual_item(TranscriptItem::SessionSummary(format!(
                    "extension {}.{} result: {output}",
                    request.id, request.command
                )));
                self.notice = Some(format!(
                    "extension {}.{} complete",
                    request.id, request.command
                ));
            }
            ExtensionOutcome::Failed(message) => {
                self.push_finalized_visual_item(TranscriptItem::Error {
                    source: format!("extension {}.{}", request.id, request.command),
                    message: message.clone(),
                });
                self.notice = Some(format!(
                    "extension {}.{} failed: {message}",
                    request.id, request.command
                ));
            }
        }
    }

    fn handle_companion_outcome(
        &mut self,
        _request: &CompanionRunRequest,
        outcome: CompanionOutcome,
        elapsed: Option<Duration>,
    ) {
        if let Some(duration) = elapsed.filter(|duration| *duration >= MIN_WORKED_DURATION) {
            self.push_finalized_visual_item(TranscriptItem::WorkedDuration(format_live_elapsed(
                duration,
            )));
        }
        match outcome {
            CompanionOutcome::Complete(result) => {
                self.push_finalized_visual_item(TranscriptItem::SessionSummary(format!(
                    "companion run result: {}",
                    serde_json::to_string(&crate::companion_run::agent_result_json(&result))
                        .unwrap_or_else(|_| "null".to_owned())
                )));
                self.notice = Some("companion run complete".to_owned());
            }
            CompanionOutcome::Failed(message) => {
                self.push_finalized_visual_item(TranscriptItem::Error {
                    source: "companion run".to_owned(),
                    message: message.clone(),
                });
                self.notice = Some(format!("companion run failed: {message}"));
            }
        }
    }

    fn refresh_patch_modal_preview(&mut self) {
        if !matches!(
            self.modal,
            Some(Modal::PatchApproval(PatchApprovalModal {
                preview: PatchPreview::Fallback(_),
                ..
            }))
        ) {
            return;
        }
        let preview = patch_approval::preview_from_events(self.transcript.events());
        if let Some(Modal::PatchApproval(modal)) = &mut self.modal {
            modal.preview = preview;
        }
    }

    fn record_in_flight_error(&mut self, event: &EventEnvelope) {
        if !self.turn_in_flight() || event.kind.as_str() != EventKind::ERROR {
            return;
        }
        let source = event
            .payload
            .get("source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("error");
        let message = event
            .payload
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("turn failed");
        self.in_flight_error = Some(format!("{source}: {message}"));
        self.interrupted_guidance = false;
    }

    fn handle_turn_outcome(&mut self, outcome: TurnOutcome, elapsed: Option<Duration>) {
        match outcome {
            TurnOutcome::Complete => {
                self.interrupted_guidance = false;
                self.in_flight_error = None;
                self.notice = None;
            }
            TurnOutcome::Cancelled => {
                self.transcript.clear_transient_live_tail();
                self.interrupted_guidance = false;
                self.in_flight_error = None;
                self.push_finalized_visual_item(TranscriptItem::Interrupted);
                self.notice = None;
            }
            TurnOutcome::Failed(message) => {
                self.interrupted_guidance = false;
                self.in_flight_error = None;
                self.transcript.clear_transient_live_tail();
                if !self.last_event_is_error() {
                    self.push_finalized_visual_item(TranscriptItem::Error {
                        source: "run_turn".to_owned(),
                        message,
                    });
                }
                self.notice = None;
            }
        }
        if let Some(elapsed) = elapsed.filter(|elapsed| *elapsed >= MIN_WORKED_DURATION) {
            self.push_finalized_visual_item(TranscriptItem::WorkedDuration(format_live_elapsed(
                elapsed,
            )));
        }
    }

    fn last_event_is_error(&self) -> bool {
        self.transcript
            .events()
            .last()
            .is_some_and(|event| event.kind.as_str() == EventKind::ERROR)
    }

    fn mark_working_timer_dirty(&mut self) -> bool {
        let Some(seconds) = self.working_elapsed_seconds() else {
            self.last_working_elapsed_secs = None;
            return false;
        };
        if self.last_working_elapsed_secs == Some(seconds) {
            return false;
        }
        self.last_working_elapsed_secs = Some(seconds);
        true
    }

    fn reply_to_modal(&mut self, reply: PermissionReply) -> CoreEffect {
        self.modal = None;
        let _ = self.reply_tx.send(reply);
        CoreEffect::Render
    }

    fn deny_open_modal(&mut self) {
        if self.modal.take().is_some() {
            let _ = self.reply_tx.send(PermissionReply::Deny);
        }
    }

    fn notice_item(&mut self, message: String) -> CoreEffect {
        self.push_notice_item(message);
        CoreEffect::Render
    }

    fn push_notice_item(&mut self, message: String) {
        self.push_finalized_visual_item(TranscriptItem::Error {
            source: "ui".to_owned(),
            message,
        });
    }

    fn summary_item(&mut self, text: String) -> CoreEffect {
        self.push_finalized_visual_item(TranscriptItem::SessionSummary(text));
        CoreEffect::Render
    }

    fn turn_status(&self) -> TurnStatus {
        match &self.state {
            AppState::TurnInFlight { .. } => TurnStatus::Running(
                self.in_flight_label
                    .clone()
                    .unwrap_or_else(|| "work".to_owned()),
            ),
            _ => TurnStatus::Idle,
        }
    }

    fn live_status_line(&self) -> Option<String> {
        if matches!(
            self.modal,
            Some(Modal::Permission(_) | Modal::PatchApproval(_))
        ) {
            return None;
        }
        if self.interrupted_guidance {
            return Some(
                "■ Conversation interrupted - tell the model what to do differently.".to_owned(),
            );
        }
        if self.in_flight_error.is_some() {
            return Some("■ Turn failed - waiting for cleanup.".to_owned());
        }
        let AppState::TurnInFlight { started_at, .. } = &self.state else {
            return None;
        };
        let label = self.in_flight_label.as_deref().unwrap_or("turn");
        if !self.is_in_flight_cancellable() {
            return Some(format!(
                "◦ Running {label} ({} • not cancellable)",
                format_live_elapsed(started_at.elapsed())
            ));
        }
        if label == "turn" {
            return Some(format!(
                "◦ Working ({} • esc to interrupt)",
                format_live_elapsed(started_at.elapsed())
            ));
        }
        Some(format!(
            "◦ Working {label} ({} • esc to interrupt)",
            format_live_elapsed(started_at.elapsed())
        ))
    }

    fn working_elapsed_seconds(&self) -> Option<u64> {
        let AppState::TurnInFlight { started_at, .. } = &self.state else {
            return None;
        };
        Some(started_at.elapsed().as_secs())
    }

    fn working_elapsed(&self) -> Option<Duration> {
        let AppState::TurnInFlight { started_at, .. } = &self.state else {
            return None;
        };
        Some(started_at.elapsed())
    }

    fn permission_ask_item(&self) -> Option<TranscriptItem> {
        let Some(Modal::Permission(request)) = &self.modal else {
            return None;
        };
        Some(TranscriptItem::PermissionAsk {
            capability: request.capability.as_str().to_owned(),
            reason: request.reason.clone(),
            command: self.shell_command_for_permission(request),
        })
    }

    fn shell_command_for_permission(&self, request: &PermissionRequest) -> Option<String> {
        if request.capability != Capability::ShellExec || request.reason != "tool run_shell" {
            return None;
        }
        let event = self.transcript.events().iter().rev().find(|event| {
            event.kind.as_str() == EventKind::TOOL_CALL
                && event
                    .payload
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    == Some("run_shell")
        })?;
        event
            .payload
            .get("input")
            .and_then(|input| input.get("command"))
            .and_then(serde_json::Value::as_str)
            .map(super::transcript::normalized_shell_command)
    }

    fn session_store(&mut self) -> Result<&SessionStore> {
        if self.session_store.is_none() {
            self.session_store = Some(resolve_session_store()?);
        }
        Ok(self
            .session_store
            .as_ref()
            .expect("session store initialized"))
    }

    fn refresh_current_session_metadata(&mut self, session_id: &str) -> Result<()> {
        self.session_store()?.refresh_session_metadata(session_id)?;
        Ok(())
    }

    fn default_export_path(&mut self, session_id: &str) -> Result<PathBuf> {
        let export_dir = self.session_store()?.home().root().join("exports");
        Ok(export_dir.join(format!("euler-session-{session_id}.json")))
    }
}

fn resolve_session_store() -> Result<SessionStore> {
    let home = EulerHome::resolve()?;
    Ok(SessionStore::new(home)?)
}

fn write_new_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    set_private_file_mode(&mut options);
    let mut file = options.open(path)?;
    file.write_all(bytes)
}

#[cfg(unix)]
fn set_private_file_mode(options: &mut fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_private_file_mode(_options: &mut fs::OpenOptions) {}

fn format_live_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn is_artifact_toggle_key(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('o') | KeyCode::Char('O'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn modal_quit_key(key: &KeyEvent) -> bool {
    key.modifiers == KeyModifiers::CONTROL
        && matches!(key.code, KeyCode::Char('c' | 'C' | 'd' | 'D'))
}

const HELP_LINES: [&str; 13] = [
    "Euler keys",
    "",
    "/        commands",
    "Shift+Enter or Alt+Enter newline",
    "Ctrl+Shift+C copy last assistant response",
    "Ctrl+O   expand/collapse tool output",
    "Ctrl+X   external editor",
    "Ctrl+C   interrupt / arm quit",
    "Ctrl+D   quit when composer empty or approval open",
    "?        show this help",
    "",
    "Any other key closes this overlay",
    "Esc closes; Ctrl+C/Ctrl+D close and keep their normal action",
];

#[cfg(test)]
mod tests;
