use self::notify::{NotifyEvent, STALL_THRESHOLD};
#[cfg(test)]
use super::app_layout::{layout, string_lines};
use super::bottom_surface::{BottomOwner, BottomSurface, SurfaceEvent};
use super::commands::CommandAction;
#[cfg(test)]
use super::composer::composer_widget;
use super::composer::{
    cursor_position_for_snapshot, desired_height_for_width, render_lines as composer_render_lines,
    ComposerLine, ComposerRenderOptions, ComposerSnapshot, OverflowIndicator, QueuedComposerLine,
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
use super::transcript::{self, TranscriptItem, TranscriptState, TOOL_CALL_MAX_LINES};
use super::tui_decider::{PermissionChannels, PermissionReply, TuiDecider};
use super::visual_canvas::{
    BlockCursor, CanvasComposerSnapshot, CanvasLine, CanvasSpan, CanvasStatusSnapshot, FocusOwner,
    TextRole, VisualBlock, VisualBlockRole, VisualCanvasFrame, VisualCanvasSnapshot,
    VisualCanvasState,
};
use crate::bundled_extensions::{
    bundled_descriptor_by_id, bundled_descriptors, bundled_extension_by_id, bundled_round_observer,
    ObserveOptions,
};
use crate::extension_enablement::{resolve_session_extensions, ExtensionSelection};
use crate::model_preference;
use anyhow::{anyhow, Result};
use crossterm::event::{self, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use euler_core::permissions::PermissionRequest;
use euler_core::{
    fold_session, heuristic_projection, load_extension_package, read_resume_prefix,
    resume_session_from_folded_prefix, AgentResult, AgentTask, ApprovalMode, EulerHome,
    ExtensionEnablement, ExtensionMaterialization, ExtensionRegistry, GrantSource, ModelTarget,
    ProvenanceWriter, ReasoningEffort, ScopePattern, Session, SessionStore,
};
use euler_event::{EventEnvelope, EventKind};
use euler_provider::catalog::MergedModelCatalog;
use euler_sdk::{Capability, Extension};
use ratatui::backend::CrosstermBackend;
#[cfg(test)]
use ratatui::layout::Rect;
use ratatui::text::Line;
#[cfg(test)]
use ratatui::widgets::Paragraph;
#[cfg(test)]
use ratatui::Frame;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::{self, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(50);
const QUIT_ARM_WINDOW: Duration = Duration::from_secs(2);
const MIN_WORKED_DURATION: Duration = Duration::from_secs(5);
const QUIT_ARM_NOTICE: &str = "ctrl+c again to quit · session saved, /resume restores";
const DENIED_COMPOSER_GHOST: &str = "denied — tell euler what to do instead";

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
#[path = "app/chrome_test.rs"]
mod chrome;
mod notify;
#[cfg(test)]
#[path = "app/render_tests_support_test.rs"]
mod render_tests_support;
mod support;
mod turn_recap;
mod visual;

#[cfg(test)]
use self::visual::{ratatui_lines_to_canvas, render_finalized_visual_items};

use self::support::{
    command_context, is_copy_key, merge_effects, read_terminal_event, session_resume_label,
    session_root_status_path, update_token_usage, CommandContextParts,
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
    /// When false, the timestamp gutter column is hidden (content widens).
    /// Defaults to true when unset.
    pub show_timestamp_gutter: Option<bool>,
    /// When false, OS notifications are suppressed. Defaults to true when unset.
    pub notifications_enabled: Option<bool>,
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
    /// Keys of foldable history items currently expanded (`history:{index}`).
    expanded_artifact_keys: HashSet<String>,
    /// Last known history viewport as `(top_row, height)` for nearest-block fold.
    last_history_viewport: (usize, usize),
    /// Foldable item spans recorded on the last history render: `(key, start, end)`.
    last_foldable_spans: Vec<(String, usize, usize)>,
    theme: Theme,
    theme_choice: ThemeChoice,
    theme_preference_path: Option<PathBuf>,
    show_timestamp_gutter: bool,
    editor: Box<dyn ExternalEditorRunner>,
    clipboard: Box<dyn ClipboardSink>,
    pending_runs: VecDeque<PendingRunRequest>,
    queued_inputs: VecDeque<String>,
    queued_selection: Option<usize>,
    queue_auto_flush_paused: bool,
    /// Empty-composer ghost override (deny-with-instruction empty path).
    empty_composer_ghost: Option<&'static str>,
    in_flight_label: Option<String>,
    /// Persona/name of the in-flight companion run, for approval panel tagging.
    in_flight_companion_name: Option<String>,
    in_flight_cancellable: bool,
    extensions: ExtensionSelection,
    observe: ObserveOptions,
    turn_event_start: usize,
    last_turn_activity_at: Option<Instant>,
    stall_notified: bool,
    terminal_focused: bool,
    notifications_enabled: bool,
    pending_notifications: VecDeque<NotifyEvent>,
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
    events_replayed: usize,
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
        self.emit_pending_notifications();
    }

    fn emit_pending_notifications(&mut self) {
        while let Some(event) = self.core.take_pending_notification() {
            let sequence = self::notify::notification_sequence(event);
            let _ = self.terminal.write_terminal_sequence(&sequence);
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
            UiAction::FocusChanged(focused) => {
                self.core.set_terminal_focused(focused);
                CoreEffect::None
            }
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
        let hard_exit = self.core.turn_in_flight();
        if hard_exit {
            self.core.deny_open_modal();
        }
        let lines = self.core.exit_recap_lines();
        terminal::restore_terminal();
        print_exit_recap_lines(&lines);
        if hard_exit {
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

fn print_exit_recap_lines(lines: &[self::turn_recap::ExitRecapLine]) {
    let stdout_tty = io::stdout().is_terminal();
    for line in lines {
        if line.is_faint() && stdout_tty {
            let _ = writeln!(io::stdout(), "\x1b[2m{}\x1b[0m", line.text());
        } else {
            let _ = writeln!(io::stdout(), "{}", line.text());
        }
    }
    let _ = io::stdout().flush();
}

struct AppCoreBootstrap {
    session_id: String,
    theme_choice: ThemeChoice,
    theme_preference_path: Option<PathBuf>,
    show_timestamp_gutter: bool,
    notifications_enabled: bool,
    model_catalog: MergedModelCatalog,
    session_store: Option<SessionStore>,
    extensions: ExtensionSelection,
    observe: ObserveOptions,
    active_session_home_managed: bool,
    theme: Theme,
    status: StatusSnapshot,
    initial_context: super::commands::CommandContext,
}

fn bootstrap_app_core(session: &Session<TuiDecider>, options: AppOptions) -> AppCoreBootstrap {
    let target = session.active_target().clone();
    let reasoning_effort = session.reasoning_effort();
    let session_id = session.session_id().to_owned();
    let cwd = session_root_status_path();
    let AppOptions {
        theme_choice,
        theme_preference_path,
        show_timestamp_gutter,
        notifications_enabled,
        model_catalog,
        session_store,
        extensions,
        observe,
        ..
    } = options;
    let show_timestamp_gutter = show_timestamp_gutter.unwrap_or(true);
    let notifications_enabled = notifications_enabled.unwrap_or(true);
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
    let initial_context = command_context(
        &model_catalog,
        &target.provider,
        &target.model,
        empty_command_context_parts(reasoning_effort, theme_choice, Some(session_id.clone())),
    );
    AppCoreBootstrap {
        session_id,
        theme_choice,
        theme_preference_path,
        show_timestamp_gutter,
        notifications_enabled,
        model_catalog,
        session_store,
        extensions,
        observe,
        active_session_home_managed,
        theme,
        status,
        initial_context,
    }
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
        let boot = bootstrap_app_core(&session, options);
        let AppCoreBootstrap {
            session_id,
            theme_choice,
            theme_preference_path,
            show_timestamp_gutter,
            notifications_enabled,
            model_catalog,
            session_store,
            extensions,
            observe,
            active_session_home_managed,
            theme,
            status,
            initial_context,
        } = boot;
        Self {
            state: AppState::Idle {
                session: Box::new(session),
            },
            permission_rx: channels.request_rx,
            reply_tx: channels.reply_tx,
            bottom: BottomSurface::new(initial_context),
            status,
            model_catalog,
            session_store,
            active_session_home_managed,
            token_usage: TokenUsageSnapshot::default(),
            transcript: TranscriptState::default(),
            visual_canvas: VisualCanvasState::new(vec![TranscriptItem::Banner {
                session_id: Some(session_id.clone()),
            }]),
            visual_scroll_offset: 0,
            composer_navigation_width: 80,
            last_working_elapsed_secs: None,
            modal: None,
            quit_armed: None,
            notice: None,
            pending_terminal_clipboard: None,
            interrupted_guidance: false,
            in_flight_error: None,
            expanded_artifact_keys: HashSet::new(),
            last_history_viewport: (0, 24),
            last_foldable_spans: Vec::new(),
            theme,
            theme_choice,
            theme_preference_path,
            show_timestamp_gutter,
            editor: Box::<SystemExternalEditor>::default(),
            clipboard: Box::<SystemClipboard>::default(),
            pending_runs: VecDeque::new(),
            queued_inputs: VecDeque::new(),
            queued_selection: None,
            queue_auto_flush_paused: false,
            empty_composer_ghost: None,
            in_flight_label: None,
            in_flight_companion_name: None,
            in_flight_cancellable: false,
            extensions,
            observe,
            turn_event_start: 0,
            last_turn_activity_at: None,
            stall_notified: false,
            terminal_focused: true,
            notifications_enabled,
            pending_notifications: VecDeque::new(),
        }
    }

    fn rebuild_bottom_surface(&mut self) {
        let (extension_items, extension_slash_commands) = self.current_extension_context();
        let parts = CommandContextParts {
            current_effort: self.current_reasoning_effort(),
            current_theme: self.theme_choice,
            current_session_id: self.status.session_id.clone(),
            checkpoint_items: self.current_checkpoint_items(),
            extension_items,
            extension_slash_commands,
        };
        self.bottom.reset_context(command_context(
            &self.model_catalog,
            &self.status.provider,
            &self.status.model,
            parts,
        ));
    }

    fn replace_bottom_surface_for_session(&mut self) {
        let (extension_items, extension_slash_commands) = self.current_extension_context();
        let parts = CommandContextParts {
            current_effort: self.current_reasoning_effort(),
            current_theme: self.theme_choice,
            current_session_id: self.status.session_id.clone(),
            checkpoint_items: self.current_checkpoint_items(),
            extension_items,
            extension_slash_commands,
        };
        self.bottom = BottomSurface::new(command_context(
            &self.model_catalog,
            &self.status.provider,
            &self.status.model,
            parts,
        ));
    }

    fn current_checkpoint_items(&self) -> Vec<crate::ui::commands::CheckpointItem> {
        let AppState::Idle { session } = &self.state else {
            return Vec::new();
        };
        session
            .workspace_checkpoints()
            .into_iter()
            .map(|item| {
                crate::ui::commands::CheckpointItem::new(
                    item.event_id,
                    item.action,
                    item.path,
                    item.ts,
                )
            })
            .collect()
    }

    fn current_extension_context(
        &self,
    ) -> (
        Vec<crate::ui::commands::ExtensionManagerItem>,
        Vec<crate::ui::commands::ExtensionSlashCommand>,
    ) {
        let session_enabled = match &self.state {
            AppState::Idle { session } => Some(session.extensions_enabled().clone()),
            _ => None,
        };
        let items = list_extension_manager_items(session_enabled.as_ref());
        let slash = crate::ui::commands::build_extension_slash_commands(&items);
        (items, slash)
    }

    fn current_reasoning_effort(&self) -> ReasoningEffort {
        self.status
            .reasoning_effort
            .as_deref()
            .and_then(ReasoningEffort::parse)
            .unwrap_or_default()
    }

    fn composer_snapshot(&self) -> ComposerSnapshot<'_> {
        ComposerSnapshot::new(self.bottom.composer())
            .with_queued(self.queued_composer_lines())
            .with_empty_ghost(self.empty_composer_ghost)
    }

    fn queued_composer_lines(&self) -> Vec<QueuedComposerLine> {
        let total = self.queued_inputs.len();
        let selected = self.selected_queue_index();
        self.queued_inputs
            .iter()
            .enumerate()
            .map(|(index, text)| QueuedComposerLine {
                position: index + 1,
                total,
                text: text.clone(),
                selected: Some(index) == selected,
            })
            .collect()
    }

    fn selected_queue_index(&self) -> Option<usize> {
        let len = self.queued_inputs.len();
        (len > 0).then(|| self.queued_selection.unwrap_or(len - 1).min(len - 1))
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
                    self.queue_auto_flush_paused = true;
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
        self.check_stall_notification();
        changed
    }

    pub fn set_terminal_focused(&mut self, focused: bool) {
        self.terminal_focused = focused;
    }

    pub fn take_pending_notification(&mut self) -> Option<NotifyEvent> {
        self.pending_notifications.pop_front()
    }

    fn queue_notification(&mut self, event: NotifyEvent) {
        if !self.notifications_enabled || self.terminal_focused {
            return;
        }
        if self.pending_notifications.back() == Some(&event) {
            return;
        }
        self.pending_notifications.push_back(event);
    }

    fn note_turn_activity(&mut self) {
        self.last_turn_activity_at = Some(Instant::now());
        self.stall_notified = false;
    }

    fn check_stall_notification(&mut self) {
        if !self.turn_in_flight() || self.stall_notified {
            return;
        }
        let Some(last) = self.last_turn_activity_at else {
            return;
        };
        if last.elapsed() < STALL_THRESHOLD {
            return;
        }
        self.stall_notified = true;
        self.queue_notification(NotifyEvent::Stall);
    }

    fn push_turn_recap(&mut self) {
        let ctx = self::turn_recap::ctx_percent(
            self.token_usage.input_tokens,
            self.token_usage.context_window_tokens,
        );
        let recap = self::turn_recap::turn_recap_from_events(
            self.transcript.events(),
            self.turn_event_start,
            ctx,
        );
        self.push_finalized_visual_item(TranscriptItem::TurnRecap {
            summary: recap.summary_line(),
            files: recap.files_line(),
        });
    }

    pub(crate) fn exit_recap_lines(&self) -> Vec<self::turn_recap::ExitRecapLine> {
        let session_id = self.status.session_id.as_deref().unwrap_or("e????");
        let events = self.transcript.events();
        self::turn_recap::exit_recap_lines(
            session_id,
            events.len(),
            self::turn_recap::session_files_changed_count(events),
        )
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
            if self.bottom.resume_picker_selected_session_id().is_some() {
                return self.preview_resume_ledger_tail();
            }
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
            KeyCode::Char('u') | KeyCode::Char('U')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.unqueue_selected_input()
            }
            KeyCode::Char('f') | KeyCode::Char('F')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.open_transcript_search()
            }
            _ if !matches!(self.bottom.owner(), BottomOwner::Composer) => {
                self.handle_surface_key(key)
            }
            KeyCode::Enter if enter_key_intent(&key) == Some(EnterKeyIntent::InsertNewline) => {
                self.edit_composer_text(|draft| draft.insert_newline())
            }
            KeyCode::Enter => self.queue_composer_input(),
            _ if is_slash_command_key(&key) && self.bottom.composer().submit_text().is_empty() => {
                self.bottom.open_palette();
                CoreEffect::Render
            }
            KeyCode::Char('@') if self.should_open_mention_picker() => {
                self.bottom.open_mention_picker(&self.status.cwd);
                CoreEffect::Render
            }
            KeyCode::Char(ch) if text_entry_modifiers(key.modifiers) => {
                self.edit_composer_text(|draft| draft.insert_char(ch))
            }
            KeyCode::Backspace => self.edit_composer_text(|draft| draft.backspace()),
            KeyCode::Delete => self.edit_composer_text(|draft| draft.delete()),
            KeyCode::Left if self.can_move_queued_selection() => self.move_queued_selection(-1),
            KeyCode::Right if self.can_move_queued_selection() => self.move_queued_selection(1),
            KeyCode::Left => self.move_composer_cursor(|draft| draft.move_left()),
            KeyCode::Right => self.move_composer_cursor(|draft| draft.move_right()),
            KeyCode::Up => self.recall_selected_queued_input(),
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
        if matches!(self.bottom.owner(), BottomOwner::Search(_)) {
            return self.handle_search_key(key);
        }
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
            KeyCode::Char(ch) if self.bottom.is_extension_manager() => {
                if let Some(event) = self.bottom.extension_manager_key(ch) {
                    return self.surface_event(event);
                }
                // Manager is not type-to-filter; ignore other chars.
                CoreEffect::None
            }
            KeyCode::Char(ch) => {
                self.bottom.palette_insert(&ch.to_string());
                if matches!(self.bottom.owner(), BottomOwner::Search(_)) {
                    self.refresh_search_matches();
                }
                CoreEffect::Render
            }
            KeyCode::Backspace => {
                let effect = self.edit_palette(BottomSurface::palette_backspace);
                if matches!(self.bottom.owner(), BottomOwner::Search(_)) {
                    self.refresh_search_matches();
                }
                effect
            }
            KeyCode::Delete => {
                let effect = self.edit_palette(BottomSurface::palette_delete);
                if matches!(self.bottom.owner(), BottomOwner::Search(_)) {
                    self.refresh_search_matches();
                }
                effect
            }
            KeyCode::Left => self.edit_palette(BottomSurface::palette_move_left),
            KeyCode::Right => self.edit_palette(BottomSurface::palette_move_right),
            KeyCode::Home => self.edit_palette(BottomSurface::palette_move_home),
            KeyCode::End => self.edit_palette(BottomSurface::palette_move_end),
            _ => CoreEffect::None,
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> CoreEffect {
        match key.code {
            KeyCode::Esc => {
                let event = self.bottom.cancel();
                self.surface_event(event)
            }
            KeyCode::Enter => {
                let previous = enter_key_intent(&key) == Some(EnterKeyIntent::InsertNewline);
                if let Some(search) = self.bottom.search_mut() {
                    if previous {
                        search.previous_match();
                    } else {
                        search.next_match();
                    }
                }
                self.scroll_to_current_search_match();
                CoreEffect::Render
            }
            KeyCode::Char(ch)
                if text_entry_modifiers(key.modifiers) || key.modifiers.is_empty() =>
            {
                self.bottom.palette_insert(&ch.to_string());
                self.refresh_search_matches();
                self.scroll_to_current_search_match();
                CoreEffect::Render
            }
            KeyCode::Backspace => {
                self.bottom.palette_backspace();
                self.refresh_search_matches();
                self.scroll_to_current_search_match();
                CoreEffect::Render
            }
            KeyCode::Delete => {
                self.bottom.palette_delete();
                self.refresh_search_matches();
                self.scroll_to_current_search_match();
                CoreEffect::Render
            }
            KeyCode::Left => self.edit_palette(BottomSurface::palette_move_left),
            KeyCode::Right => self.edit_palette(BottomSurface::palette_move_right),
            KeyCode::Home => self.edit_palette(BottomSurface::palette_move_home),
            KeyCode::End => self.edit_palette(BottomSurface::palette_move_end),
            _ => CoreEffect::None,
        }
    }

    fn refresh_search_matches(&mut self) {
        let lines = self.search_haystack_lines();
        if let Some(search) = self.bottom.search_mut() {
            search.recompute(&lines);
        }
    }

    fn search_haystack_lines(&self) -> Vec<String> {
        // Plain text of finalized ledger history rows — the same set the
        // visual canvas projects. Not live streaming markdown only.
        let width = self.composer_navigation_width.max(40);
        let items = self.visual_canvas.finalized_items().to_vec();
        let lines = crate::ui::text::with_timestamp_gutter(self.show_timestamp_gutter, || {
            transcript::render_items_for_history(&items, &self.theme, width)
        });
        lines
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn scroll_to_current_search_match(&mut self) {
        let Some(line_index) = self
            .bottom
            .search()
            .and_then(|search| search.current_match())
            .map(|m| m.line_index)
        else {
            return;
        };
        let total = self.search_haystack_lines().len();
        let height = self.last_history_viewport.1.max(1);
        // visual_scroll_offset is rows above the bottom-aligned tail.
        let bottom_start = total.saturating_sub(height);
        if line_index >= bottom_start {
            self.visual_scroll_offset = 0;
        } else {
            self.visual_scroll_offset = bottom_start.saturating_sub(line_index);
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
            KeyCode::Char('@') if self.should_open_mention_picker() => {
                self.bottom.open_mention_picker(&self.status.cwd);
                CoreEffect::Render
            }
            KeyCode::Char(ch) => self.edit_composer_text(|draft| draft.insert_char(ch)),
            KeyCode::Backspace => self.edit_composer_text(|draft| draft.backspace()),
            KeyCode::Delete => self.edit_composer_text(|draft| draft.delete()),
            KeyCode::Left if self.can_move_queued_selection() => self.move_queued_selection(-1),
            KeyCode::Right if self.can_move_queued_selection() => self.move_queued_selection(1),
            KeyCode::Left => self.move_composer_cursor(|draft| draft.move_left()),
            KeyCode::Right => self.move_composer_cursor(|draft| draft.move_right()),
            KeyCode::Up => self.move_composer_up_or_history(),
            KeyCode::Down => self.move_composer_down_or_history(),
            KeyCode::Home => self.move_composer_cursor(|draft| draft.move_home()),
            KeyCode::End => self.move_composer_cursor(|draft| draft.move_end()),
            _ => CoreEffect::None,
        }
    }

    fn should_open_mention_picker(&self) -> bool {
        // Open when `@` starts a token (start of draft or after whitespace).
        let text = self.bottom.composer().render_text();
        let cursor = self.bottom.composer().cursor_offset();
        if cursor == 0 {
            return true;
        }
        let units: Vec<char> = text.chars().collect();
        units
            .get(cursor.saturating_sub(1))
            .is_some_and(|ch| ch.is_whitespace())
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
            KeyCode::Char('u') | KeyCode::Char('U')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                Some(self.unqueue_selected_input())
            }
            KeyCode::Char('f') | KeyCode::Char('F')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                Some(self.open_transcript_search())
            }
            KeyCode::Esc => Some(CoreEffect::None),
            _ => None,
        }
    }

    fn open_transcript_search(&mut self) -> CoreEffect {
        self.disarm_quit_notice();
        self.bottom.open_search();
        self.refresh_search_matches();
        CoreEffect::Render
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
        self.empty_composer_ghost = None;
        self.bottom.edit_composer(|draft| {
            let _ = draft.insert_bracketed_paste(text);
        });
        CoreEffect::Render
    }

    fn handle_modal_input(&mut self, input: InputEvent) -> CoreEffect {
        let InputEvent::Key(key) = input else {
            return self.handle_modal_composer_input(input);
        };
        if matches!(self.modal, Some(Modal::PatchApproval(_))) {
            return self.handle_patch_modal_key(key);
        }
        self.handle_approval_modal_key(key)
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
        self.handle_approval_modal_key(key)
    }

    fn handle_approval_modal_key(&mut self, key: KeyEvent) -> CoreEffect {
        // Hotkeys fire only when the composer draft is empty. Once the user
        // starts typing a denial instruction, y/a/p/n insert text; only Esc
        // (deny with the typed instruction) or a quit chord decide.
        let draft_empty = self.bottom.composer().submit_text().is_empty();
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') if draft_empty => {
                self.reply_to_modal(PermissionReply::AllowOnce)
            }
            KeyCode::Char('a') | KeyCode::Char('A') if draft_empty => {
                let prefix = self.modal_scope_prefix().unwrap_or_default();
                self.reply_to_modal(PermissionReply::AllowSessionScope(prefix))
            }
            KeyCode::Char('p') | KeyCode::Char('P') if draft_empty => {
                let prefix = self.modal_scope_prefix().unwrap_or_default();
                self.reply_to_modal(PermissionReply::AllowProjectScope(prefix))
            }
            KeyCode::Char('n') | KeyCode::Char('N') if draft_empty => self.reply_deny_from_modal(),
            KeyCode::Esc => self.reply_deny_from_modal(),
            _ if modal_quit_key(&key) => {
                // Quit path: bare deny only — do not queue a follow-up turn.
                self.reply_to_modal(PermissionReply::Deny);
                CoreEffect::Quit
            }
            _ => self.handle_modal_composer_key(key),
        }
    }

    fn modal_scope_prefix(&self) -> Option<String> {
        let request = match &self.modal {
            Some(Modal::Permission(request)) => request,
            Some(Modal::PatchApproval(modal)) => &modal.request,
            None | Some(Modal::Help) => return None,
        };
        patch_approval::derive_scope_prefix(request)
    }

    fn reply_deny_from_modal(&mut self) -> CoreEffect {
        let draft = self.bottom.composer().submit_text();
        if draft.trim().is_empty() {
            self.empty_composer_ghost = Some(DENIED_COMPOSER_GHOST);
            self.reply_to_modal(PermissionReply::Deny)
        } else {
            self.bottom.replace_composer_text("");
            self.empty_composer_ghost = None;
            // Front of queue: next user turn after the denied tool turn finishes.
            self.queued_inputs.push_front(draft.clone());
            self.queued_selection = Some(0);
            self.reply_to_modal(PermissionReply::DenyWithInstruction(draft))
        }
    }

    fn handle_modal_composer_input(&mut self, input: InputEvent) -> CoreEffect {
        match input {
            InputEvent::Paste(text) => {
                self.empty_composer_ghost = None;
                self.bottom.edit_composer(|draft| {
                    let _ = draft.insert_bracketed_paste(&text);
                });
                CoreEffect::Render
            }
            InputEvent::Key(key) => self.handle_modal_composer_key(key),
            InputEvent::Mouse(_) => CoreEffect::None,
        }
    }

    fn handle_modal_composer_key(&mut self, key: KeyEvent) -> CoreEffect {
        match key.code {
            KeyCode::Enter if enter_key_intent(&key) == Some(EnterKeyIntent::InsertNewline) => {
                self.edit_composer_text(|draft| draft.insert_newline())
            }
            KeyCode::Char(ch) if text_entry_modifiers(key.modifiers) => {
                self.edit_composer_text(|draft| draft.insert_char(ch))
            }
            KeyCode::Backspace => self.edit_composer_text(|draft| draft.backspace()),
            KeyCode::Delete => self.edit_composer_text(|draft| draft.delete()),
            KeyCode::Left => self.move_composer_cursor(|draft| draft.move_left()),
            KeyCode::Right => self.move_composer_cursor(|draft| draft.move_right()),
            KeyCode::Home => self.move_composer_cursor(|draft| draft.move_home()),
            KeyCode::End => self.move_composer_cursor(|draft| draft.move_end()),
            _ => CoreEffect::None,
        }
    }

    fn handle_submit(&mut self) -> CoreEffect {
        // Mention segments submit as workspace-relative paths (file references
        // in the user.message content). A dedicated context.slot.updated path
        // is deferred until core exposes a non-extension slot writer — do not
        // invent a parallel canvas channel here.
        let prompt = self.bottom.composer().submit_text();
        if prompt.trim().is_empty() {
            return self.continue_queued_input();
        }
        let AppState::Idle { .. } = self.state else {
            return self.queue_composer_input();
        };
        self.visual_scroll_offset = 0;
        self.queue_auto_flush_paused = false;
        self.bottom.record_submission(&prompt);
        let session = self.take_idle_session();
        self.rebuild_bottom_surface();
        self.spawn_turn(prompt, session);
        CoreEffect::Render
    }

    fn queue_composer_input(&mut self) -> CoreEffect {
        let prompt = self.bottom.composer().submit_text();
        if prompt.trim().is_empty() {
            return CoreEffect::None;
        }
        self.queued_inputs.push_back(prompt);
        self.queued_selection = self.queued_inputs.len().checked_sub(1);
        self.bottom.replace_composer_text("");
        self.notice = None;
        CoreEffect::Render
    }

    fn continue_queued_input(&mut self) -> CoreEffect {
        let AppState::Idle { .. } = self.state else {
            return CoreEffect::None;
        };
        let Some(prompt) = self.pop_next_queued_input() else {
            return CoreEffect::None;
        };
        self.queue_auto_flush_paused = false;
        self.visual_scroll_offset = 0;
        self.bottom.record_submission(&prompt);
        let session = self.take_idle_session();
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
        self.in_flight_companion_name = None;
        self.in_flight_cancellable = true;
        self.last_working_elapsed_secs = None;
        self.interrupted_guidance = false;
        self.in_flight_error = None;
        self.turn_event_start = self.transcript.events().len();
        self.note_turn_activity();
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
            CommandAction::ShowCompaction => self.show_compaction_status(),
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
            CommandAction::OpenPermissions => self.open_permissions_picker(),
            CommandAction::RevokeGrant {
                capability,
                pattern,
                source,
            } => self.revoke_grant(capability, pattern, source),
            CommandAction::ShowHelp { text } => self.summary_item(text),
            CommandAction::ResumeSession { session_id } => {
                self.resume_session_from_picker(session_id)
            }
            CommandAction::RollbackCheckpoint { event_id } => {
                self.rollback_workspace_checkpoint(event_id)
            }
            CommandAction::ScrollViewportToBottom => {
                self.transcript.scroll_to_bottom();
                self.visual_scroll_offset = 0;
                CoreEffect::Render
            }
            CommandAction::CopyLastAssistantResponse => self.copy_last_assistant_response(),
            CommandAction::NameSession { name } => self.name_current_session(name),
            CommandAction::ToggleTimestamps => self.toggle_timestamps(),
            CommandAction::ShowDiff => self.show_session_diff(),
            CommandAction::ShowUsage => self.show_session_usage(),
            CommandAction::DagExport => self.dag_export(),
            CommandAction::OpenExtensionManager => self.open_extension_manager(),
            CommandAction::ExtensionToggle { id, enable } => self.toggle_extension(id, enable),
            CommandAction::ExtensionDetails { id } => self.show_extension_details(id),
            CommandAction::ExtensionRemove { id } => self.remove_extension(id),
            CommandAction::ExtensionAdd { path } => self.add_extension(path),
        }
    }

    fn toggle_timestamps(&mut self) -> CoreEffect {
        self.show_timestamp_gutter = !self.show_timestamp_gutter;
        if let Some(path) = self.theme_preference_path.as_deref() {
            if let Err(error) =
                model_preference::save_timestamps_preference(path, self.show_timestamp_gutter)
            {
                self.push_notice_item(format!(
                    "timestamps {}; preference not saved: {error}",
                    if self.show_timestamp_gutter {
                        "shown"
                    } else {
                        "hidden"
                    }
                ));
                return CoreEffect::Render;
            }
        }
        // Faint confirmation line; also logged as a transcript notice item.
        let message = if self.show_timestamp_gutter {
            "timestamps shown".to_owned()
        } else {
            "timestamps hidden".to_owned()
        };
        self.notice_item(message)
    }

    fn show_session_diff(&mut self) -> CoreEffect {
        let AppState::Idle { session } = &self.state else {
            return self.notice_item("diff waits for the active turn".to_owned());
        };
        let diffs = session_attributed_diffs(session.events());
        if diffs.is_empty() {
            return self.summary_item("no session-attributed file changes yet".to_owned());
        }
        let mut header = format!("session diff · {} file(s)", diffs.len());
        let mut total_added = 0usize;
        let mut total_removed = 0usize;
        for diff in &diffs {
            let (a, r) = count_diff_lines(diff.diff.as_deref().unwrap_or(""));
            total_added += a;
            total_removed += r;
        }
        header.push_str(&format!(" · +{total_added} −{total_removed}"));
        self.push_finalized_visual_item(TranscriptItem::SessionSummary(header));
        for diff in diffs {
            self.push_finalized_visual_item(TranscriptItem::FileDiff {
                path: diff.path,
                action: diff.action,
                origin: "session".to_owned(),
                diff: diff.diff,
                truncated: diff.truncated,
                truncation: diff.truncation,
                omitted_reason: diff.omitted_reason,
                checkpoint_event_id: None,
            });
        }
        CoreEffect::Render
    }

    fn show_session_usage(&mut self) -> CoreEffect {
        // Cost display is deferred until provider price catalogs exist.
        let AppState::Idle { session } = &self.state else {
            // Fall back to live snapshot when a turn is in flight.
            let text = format_usage_from_snapshot(&self.token_usage, &self.status);
            return self.summary_item(text);
        };
        let text = format_session_usage(session.events(), &self.status, &self.token_usage);
        self.summary_item(text)
    }

    fn dag_export(&mut self) -> CoreEffect {
        let enabled = match &self.state {
            AppState::Idle { session } => session.extension_enabled("causal-dag"),
            _ => {
                // Check registry/session context when not idle.
                self.current_extension_context()
                    .0
                    .iter()
                    .find(|item| item.id == "causal-dag")
                    .is_some_and(|item| item.enabled)
            }
        };
        if !enabled {
            return self.notice_item(crate::ui::commands::disabled_extension_teach(
                "/dag",
                "causal-dag",
            ));
        }
        self.extension_run(
            "causal-dag".to_owned(),
            "export".to_owned(),
            serde_json::Value::Object(serde_json::Map::new()),
        )
    }

    fn open_extension_manager(&mut self) -> CoreEffect {
        self.rebuild_bottom_surface();
        self.bottom.open_extension_manager();
        CoreEffect::Render
    }

    fn toggle_extension(&mut self, id: String, enable: bool) -> CoreEffect {
        match set_extension_enabled(&id, enable) {
            Ok(()) => {
                if let AppState::Idle { session } = &mut self.state {
                    session.set_extension_enabled(&id, enable);
                }
                self.rebuild_bottom_surface();
                let verb = if enable { "enabled" } else { "disabled" };
                // Decision-record line in the ledger.
                self.push_notice_item(format!("✓ extension {verb}: {id}"));
                self.bottom.open_extension_manager();
                CoreEffect::Render
            }
            Err(error) => self.notice_item(format!("extension toggle failed: {error}")),
        }
    }

    fn show_extension_details(&mut self, id: String) -> CoreEffect {
        let (items, _) = self.current_extension_context();
        match items.into_iter().find(|item| item.id == id) {
            Some(item) => self.summary_item(item.details_text()),
            None => self.notice_item(format!("unknown extension: {id}")),
        }
    }

    fn remove_extension(&mut self, id: String) -> CoreEffect {
        match remove_linked_extension(&id) {
            Ok(message) => {
                if let AppState::Idle { session } = &mut self.state {
                    session.set_extension_enabled(&id, false);
                }
                self.rebuild_bottom_surface();
                self.push_notice_item(format!("✓ extension removed: {id} · {message}"));
                CoreEffect::Render
            }
            Err(error) => self.notice_item(format!("extension remove failed: {error}")),
        }
    }

    fn add_extension(&mut self, path: String) -> CoreEffect {
        match add_local_extension(std::path::Path::new(&path)) {
            Ok(report) => {
                if let AppState::Idle { session } = &mut self.state {
                    session.set_extension_enabled(&report.id, true);
                }
                self.rebuild_bottom_surface();
                self.push_notice_item(format!(
                    "✓ extension installed · {} · enabled for session",
                    report.id
                ));
                self.summary_item(report.steps_text());
                CoreEffect::Render
            }
            Err(error) => self.notice_item(format!("extension add failed: {error}")),
        }
    }

    fn rollback_workspace_checkpoint(&mut self, event_id: String) -> CoreEffect {
        let AppState::Idle { session } = &mut self.state else {
            return self.notice_item("rollback waits for the active turn".to_owned());
        };
        let prior_len = session.events().len();
        match session.restore_workspace_checkpoint(&event_id) {
            Ok(outcome) => {
                let new_events = session.events()[prior_len..].to_vec();
                for event in new_events {
                    self.transcript.push_event(event);
                    self.queue_finalized_visual_output_for_latest_event();
                }
                self.rebuild_bottom_surface();
                self.notice = Some(format!(
                    "restored {} from checkpoint {}",
                    outcome.path, outcome.checkpoint_event_id
                ));
                CoreEffect::Render
            }
            Err(error) => self.notice_item(format!("rollback failed: {error}")),
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
        self.expanded_artifact_keys.clear();
        self.last_foldable_spans.clear();
        self.modal = None;
        self.quit_armed = None;
        self.last_working_elapsed_secs = None;
        self.interrupted_guidance = false;
        self.in_flight_error = None;
        self.clear_queued_inputs();
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

    fn show_compaction_status(&mut self) -> CoreEffect {
        let AppState::Idle { session } = &self.state else {
            return self.notice_item("compaction status waits for the active turn".to_owned());
        };
        let policy = session.auto_compaction_policy();
        let demoted = self.token_usage.demoted_items;
        let retained = self
            .token_usage
            .canvas_retained_bytes
            .map(|bytes| bytes.to_string())
            .unwrap_or_else(|| "?".to_owned());
        let limit = session
            .context_limit_tokens()
            .map(|tokens| tokens.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let used = session
            .latest_model_usage_used_tokens()
            .map(|tokens| tokens.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        self.notice_item(format!(
            "compaction tier={} budget_bytes={} retained_bytes={retained} demoted={demoted} limit_tokens={limit} used_tokens={used} reserve={}",
            policy.tier.as_str(),
            policy.budget_bytes,
            session.compaction_reserve_tokens()
        ))
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
        self.in_flight_companion_name = None;
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
        self.in_flight_companion_name = Some(request.task.persona().to_owned());
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
                // Faint line above composer; composer/queue input stays intact.
                self.notice = Some("resume waits for the active turn".to_owned());
                return CoreEffect::Render;
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

    fn preview_resume_ledger_tail(&mut self) -> CoreEffect {
        let Some(session_id) = self.bottom.resume_picker_selected_session_id() else {
            return CoreEffect::None;
        };
        match self.load_resume_ledger_tail_preview(&session_id) {
            Ok(lines) => {
                self.bottom.set_resume_ledger_preview(lines);
                CoreEffect::Render
            }
            Err(error) => {
                self.notice = Some(format!("preview failed: {error}"));
                CoreEffect::Render
            }
        }
    }

    fn load_resume_ledger_tail_preview(&mut self, session_id: &str) -> Result<Vec<String>> {
        const PREVIEW_TAIL_LINES: usize = 16;
        let record = self
            .session_store()?
            .find_session(session_id)?
            .ok_or_else(|| anyhow!("no session found with id {session_id}"))?;
        let events = read_resume_prefix(record.events_path())?;
        let items = transcript::project_events(&events);
        let width = self.composer_navigation_width.max(40);
        let rendered = crate::ui::text::with_timestamp_gutter(self.show_timestamp_gutter, || {
            transcript::render_items_for_history(&items, &self.theme, width)
        });
        let lines: Vec<String> = rendered
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let start = lines.len().saturating_sub(PREVIEW_TAIL_LINES);
        Ok(lines[start..].to_vec())
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
        // Compaction window follows the active model after fold (post-switch).
        config.provider = folded.active_target.provider.clone();
        config.model = folded.active_target.model.clone();
        crate::session_lifecycle::apply_catalog_context_limit(&mut config, &self.model_catalog);
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
        let events_replayed = outcome.events_folded;
        Ok(TuiResume {
            session,
            channels,
            events,
            active_target: outcome.active_target,
            display_label: session_resume_label(&record),
            recovery_closure_appended: outcome.recovery_closure_appended,
            warning_count: outcome.warnings.len(),
            events_replayed,
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
        // Rebuild first so token_usage reflects the resumed event stream under
        // the post-fold active model window (footer ctx% = model budget).
        self.rebuild_transcript_from_events(&resume.events);
        self.token_usage.context_window_tokens = self.active_context_window_tokens();
        let label = if resume.display_label == "Untitled session" {
            session_id.clone()
        } else {
            resume.display_label.clone()
        };
        self.push_finalized_visual_item(TranscriptItem::ResumeBoundary {
            label,
            recovery_closure_appended: resume.recovery_closure_appended,
            warning_count: resume.warning_count,
            events_replayed: resume.events_replayed,
        });
        self.visual_scroll_offset = 0;
        self.modal = None;
        self.quit_armed = None;
        self.last_working_elapsed_secs = None;
        self.interrupted_guidance = false;
        self.in_flight_error = None;
        self.clear_queued_inputs();
        self.notice = None;
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
        let mut finalized = vec![TranscriptItem::Banner {
            session_id: self.status.session_id.clone(),
        }];
        finalized.extend(transcript.items());
        self.transcript = transcript;
        self.token_usage = token_usage;
        self.visual_canvas = VisualCanvasState::new(finalized);
    }

    fn switch_model(&mut self, provider: String, model: String) -> CoreEffect {
        let AppState::Idle { session } = &mut self.state else {
            return self.notice_item("model switch waits for the active turn".to_owned());
        };
        let context_limit = self
            .model_catalog
            .provider(&provider)
            .and_then(|descriptor| {
                descriptor
                    .models()
                    .find(|entry| entry.id() == model)
                    .and_then(|entry| entry.context_window_tokens())
            })
            .and_then(euler_core::ContextLimitConfig::from_catalog_window);
        match session.switch_model(&provider, &model, "user", context_limit) {
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

    fn open_permissions_picker(&mut self) -> CoreEffect {
        let AppState::Idle { session } = &self.state else {
            return self.notice_item("permissions wait for the active turn".to_owned());
        };
        let grants = session.list_grants();
        let choices = super::commands::permission_choices_with_grants(&grants);
        self.bottom
            .open_picker(super::commands::PickerSpec::Permissions(choices));
        CoreEffect::Render
    }

    fn revoke_grant(
        &mut self,
        capability: Capability,
        pattern: String,
        source: GrantSource,
    ) -> CoreEffect {
        let AppState::Idle { session } = &mut self.state else {
            return self.notice_item("revoke waits for the active turn".to_owned());
        };
        let pattern = match ScopePattern::new(pattern) {
            Ok(pattern) => pattern,
            Err(error) => {
                return self.notice_item(format!("invalid grant pattern: {error}"));
            }
        };
        match session.revoke_grant(capability, &pattern, source) {
            Ok(0) => self.notice_item(format!(
                "no {} grant for {} ({})",
                source.as_str(),
                capability.as_str(),
                if pattern.is_unscoped() {
                    "all"
                } else {
                    pattern.as_str()
                }
            )),
            Ok(_) => self.notice_item(format!(
                "revoked {} {} ({})",
                source.as_str(),
                capability.as_str(),
                if pattern.is_unscoped() {
                    "all"
                } else {
                    pattern.as_str()
                }
            )),
            Err(error) => self.notice_item(format!("revoke failed: {error}")),
        }
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
        self.empty_composer_ghost = None;
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

    fn recall_selected_queued_input(&mut self) -> CoreEffect {
        let Some(index) = self.selected_queue_index() else {
            return self.move_composer_up_or_history();
        };
        if !self.bottom.composer().submit_text().is_empty() {
            return self.move_composer_up_or_history();
        }
        let Some(text) = self.queued_inputs.remove(index) else {
            return CoreEffect::None;
        };
        self.bottom.replace_composer_text(&text);
        self.normalize_queue_selection();
        CoreEffect::Render
    }

    fn unqueue_selected_input(&mut self) -> CoreEffect {
        let Some(index) = self.selected_queue_index() else {
            return CoreEffect::None;
        };
        self.queued_inputs.remove(index);
        self.normalize_queue_selection();
        CoreEffect::Render
    }

    fn can_move_queued_selection(&self) -> bool {
        self.bottom.composer().submit_text().is_empty() && self.queued_inputs.len() > 1
    }

    fn move_queued_selection(&mut self, delta: isize) -> CoreEffect {
        let Some(index) = self.selected_queue_index() else {
            return CoreEffect::None;
        };
        let last = self.queued_inputs.len() - 1;
        self.queued_selection = Some(index.saturating_add_signed(delta).min(last));
        CoreEffect::Render
    }

    fn pop_next_queued_input(&mut self) -> Option<String> {
        let prompt = self.queued_inputs.pop_front();
        self.normalize_queue_selection();
        prompt
    }

    fn normalize_queue_selection(&mut self) {
        self.queued_selection = self
            .selected_queue_index()
            .or_else(|| self.queued_inputs.len().checked_sub(1));
    }

    fn clear_queued_inputs(&mut self) {
        self.queued_inputs.clear();
        self.queued_selection = None;
        self.queue_auto_flush_paused = false;
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
            .has_foldable_artifact(TOOL_CALL_MAX_LINES)
        {
            return CoreEffect::None;
        }
        self.refresh_foldable_spans(self.composer_navigation_width);
        let Some(key) = self.nearest_foldable_key() else {
            return CoreEffect::None;
        };
        if !self.expanded_artifact_keys.remove(&key) {
            self.expanded_artifact_keys.insert(key);
        }
        self.visual_canvas.invalidate_history_cache();
        self.visual_scroll_offset = 0;
        CoreEffect::ReplayHistoryWithScrollbackPurge
    }

    fn nearest_foldable_key(&self) -> Option<String> {
        let spans = &self.last_foldable_spans;
        if spans.is_empty() {
            return None;
        }
        let (top, height) = self.last_history_viewport;
        let height = height.max(1);
        let center = top.saturating_add(height / 2);
        let mut best: Option<(usize, usize, &str)> = None;
        for (index, (key, start, end)) in spans.iter().enumerate() {
            let mid = start.saturating_add(end.saturating_sub(*start) / 2);
            let dist = mid.abs_diff(center);
            let rank = (dist, spans.len() - 1 - index);
            match best {
                Some((best_dist, best_rev, _)) if rank >= (best_dist, best_rev) => {}
                _ => best = Some((rank.0, rank.1, key.as_str())),
            }
        }
        best.map(|(_, _, key)| key.to_owned())
    }

    fn refresh_foldable_spans(&mut self, width: u16) {
        let items = self.visual_canvas.finalized_items().to_vec();
        let theme = self.theme.clone();
        let mut row = 0usize;
        let mut spans = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let key = transcript::artifact_key_for_index(index);
            let limit = if self.expanded_artifact_keys.contains(&key) {
                usize::MAX
            } else {
                TOOL_CALL_MAX_LINES
            };
            let line_count = transcript::render_items_for_history_with_limit(
                std::slice::from_ref(item),
                &theme,
                width,
                limit,
            )
            .len();
            if item.is_foldable_artifact(TOOL_CALL_MAX_LINES) {
                let end = row.saturating_add(line_count.saturating_sub(1));
                spans.push((key, row, end));
            }
            row = row.saturating_add(line_count);
        }
        let height = self.last_history_viewport.1.max(1);
        let top = row
            .saturating_sub(height)
            .saturating_sub(self.visual_scroll_offset);
        self.last_history_viewport = (top, height);
        self.last_foldable_spans = spans;
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
                    self.queue_notification(NotifyEvent::ApprovalNeeded);
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
                self.note_turn_activity();
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
                let auto_flush = outcome == TurnOutcome::Complete;
                self.last_working_elapsed_secs = None;
                self.handle_turn_outcome(outcome, elapsed);
                self.accept_worker_session_or_continue(session, auto_flush);
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
                let auto_flush = matches!(&outcome, ExtensionOutcome::Complete(_));
                self.handle_extension_outcome(&request, outcome, elapsed);
                self.accept_worker_session_or_continue(session, auto_flush);
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
                let auto_flush = matches!(&outcome, CompanionOutcome::Complete(_));
                self.handle_companion_outcome(&request, outcome, elapsed);
                self.accept_worker_session_or_continue(session, auto_flush);
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

    fn accept_worker_session_or_continue(
        &mut self,
        session: Box<Session<TuiDecider>>,
        auto_flush: bool,
    ) {
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
            return;
        }
        if auto_flush && !self.queue_auto_flush_paused {
            if let Some(prompt) = self.pop_next_queued_input() {
                self.bottom.record_submission(&prompt);
                self.spawn_turn(prompt, session);
                return;
            }
        }
        self.state = AppState::Idle { session };
        self.in_flight_label = None;
        self.in_flight_companion_name = None;
        self.in_flight_cancellable = false;
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
        let emit_recap = match &outcome {
            TurnOutcome::Complete => {
                self.interrupted_guidance = false;
                self.in_flight_error = None;
                self.notice = None;
                true
            }
            TurnOutcome::Cancelled => {
                self.queue_auto_flush_paused = true;
                self.transcript.clear_transient_live_tail();
                self.interrupted_guidance = false;
                self.in_flight_error = None;
                self.push_finalized_visual_item(TranscriptItem::Interrupted);
                self.notice = None;
                false
            }
            TurnOutcome::Failed(message) => {
                self.queue_auto_flush_paused = true;
                self.interrupted_guidance = false;
                self.in_flight_error = None;
                self.transcript.clear_transient_live_tail();
                if !self.last_event_is_error() {
                    self.push_finalized_visual_item(TranscriptItem::Error {
                        source: "run_turn".to_owned(),
                        message: message.clone(),
                    });
                }
                self.notice = None;
                true
            }
        };
        if let Some(elapsed) = elapsed.filter(|elapsed| *elapsed >= MIN_WORKED_DURATION) {
            self.push_finalized_visual_item(TranscriptItem::WorkedDuration(format_live_elapsed(
                elapsed,
            )));
        }
        if emit_recap {
            self.push_turn_recap();
        }
        match outcome {
            TurnOutcome::Complete => self.queue_notification(NotifyEvent::TurnDone),
            TurnOutcome::Failed(_) => self.queue_notification(NotifyEvent::Failure),
            TurnOutcome::Cancelled => {}
        }
        self.last_turn_activity_at = None;
        self.stall_notified = false;
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
            return Some("■ interrupted — tell euler what to do differently".to_owned());
        }
        if self.in_flight_error.is_some() {
            return Some("■ turn failed — waiting for cleanup".to_owned());
        }
        let AppState::TurnInFlight { started_at, .. } = &self.state else {
            return None;
        };
        let secs = started_at.elapsed().as_secs();
        let label = self.in_flight_label.as_deref().unwrap_or("turn");
        if !self.is_in_flight_cancellable() {
            return Some(format!("⠧ running {label} · {secs}s · not cancellable"));
        }
        if label == "turn" {
            return Some(format!("⠧ working · {secs}s · esc to interrupt"));
        }
        Some(format!("⠧ working {label} · {secs}s · esc to interrupt"))
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
            command: self
                .shell_command_for_permission(request)
                .or_else(|| request.command.clone()),
            scope_prefix: patch_approval::derive_scope_prefix(request),
            companion_name: self.in_flight_companion_name.clone(),
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

fn empty_command_context_parts(
    current_effort: ReasoningEffort,
    current_theme: ThemeChoice,
    current_session_id: Option<String>,
) -> CommandContextParts {
    CommandContextParts {
        current_effort,
        current_theme,
        current_session_id,
        checkpoint_items: Vec::new(),
        extension_items: Vec::new(),
        extension_slash_commands: Vec::new(),
    }
}

struct SessionDiffEntry {
    path: String,
    action: String,
    diff: Option<String>,
    truncated: bool,
    truncation: String,
    omitted_reason: Option<String>,
}

/// Latest `file.diff` per path for files this session touched (not full WT).
fn session_attributed_diffs(events: &[EventEnvelope]) -> Vec<SessionDiffEntry> {
    use std::collections::BTreeMap;
    let mut latest: BTreeMap<String, SessionDiffEntry> = BTreeMap::new();
    for event in events {
        if event.kind.as_str() != EventKind::FILE_DIFF {
            continue;
        }
        let path = event
            .payload
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        if path.is_empty() {
            continue;
        }
        let action = event
            .payload
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("modify")
            .to_owned();
        let diff = event
            .payload
            .get("diff")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let truncated = event
            .payload
            .get("truncated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let truncation = event
            .payload
            .get("truncation")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let omitted_reason = event
            .payload
            .get("omitted_reason")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        latest.insert(
            path.clone(),
            SessionDiffEntry {
                path,
                action,
                diff,
                truncated,
                truncation,
                omitted_reason,
            },
        );
    }
    latest.into_values().collect()
}

fn count_diff_lines(diff: &str) -> (usize, usize) {
    let mut added = 0usize;
    let mut removed = 0usize;
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }
    (added, removed)
}

fn format_usage_from_snapshot(tokens: &TokenUsageSnapshot, status: &StatusSnapshot) -> String {
    let mut lines = vec![
        format!("usage · {}::{}", status.provider, status.model),
        format!("  input:     {} tokens", tokens.input_tokens),
        format!("  output:    {} tokens", tokens.output_tokens),
    ];
    if let Some(reasoning) = tokens.reasoning_tokens {
        lines.push(format!("  reasoning: {reasoning} tokens"));
    }
    lines.push("  cost:      (catalog prices unavailable — tokens only)".to_owned());
    lines.join("\n")
}

fn format_session_usage(
    events: &[EventEnvelope],
    status: &StatusSnapshot,
    live: &TokenUsageSnapshot,
) -> String {
    use std::collections::BTreeMap;
    #[derive(Default)]
    struct Bucket {
        input: u64,
        output: u64,
        reasoning: u64,
        calls: u64,
    }
    let mut by_model: BTreeMap<(String, String), Bucket> = BTreeMap::new();
    for event in events {
        if event.kind.as_str() != EventKind::MODEL_RESULT {
            continue;
        }
        let provider = event
            .payload
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_owned();
        let model = event
            .payload
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_owned();
        let usage = event.payload.get("usage").and_then(|v| v.as_object());
        let input = usage
            .and_then(|u| u.get("input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output = usage
            .and_then(|u| u.get("output_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let reasoning = usage
            .and_then(|u| u.get("reasoning_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let bucket = by_model.entry((provider, model)).or_default();
        bucket.input += input;
        bucket.output += output;
        bucket.reasoning += reasoning;
        bucket.calls += 1;
    }
    if by_model.is_empty() {
        return format_usage_from_snapshot(live, status);
    }
    let mut lines = vec!["usage · session totals (no catalog prices)".to_owned()];
    for ((provider, model), bucket) in by_model {
        lines.push(format!("{provider}::{model} · {} call(s)", bucket.calls));
        lines.push(format!("  input:     {} tokens", bucket.input));
        lines.push(format!("  output:    {} tokens", bucket.output));
        if bucket.reasoning > 0 {
            lines.push(format!("  reasoning: {} tokens", bucket.reasoning));
        }
    }
    lines.join("\n")
}

fn list_extension_manager_items(
    session_enabled: Option<&std::collections::BTreeSet<String>>,
) -> Vec<crate::ui::commands::ExtensionManagerItem> {
    let Ok(home) = EulerHome::resolve() else {
        return Vec::new();
    };
    let registry = ExtensionRegistry::open_read_only(home);
    let enablement = registry.enablement_states().unwrap_or_default();
    let audit_by_id = audit_status_by_id(&registry);
    let mut items = bundled_manager_items(session_enabled, &enablement, &audit_by_id);
    append_linked_manager_items(
        &mut items,
        &registry,
        session_enabled,
        &enablement,
        &audit_by_id,
    );
    items
}

fn audit_status_by_id(registry: &ExtensionRegistry) -> std::collections::BTreeMap<String, String> {
    registry
        .audit()
        .ok()
        .map(|report| {
            report
                .entries
                .into_iter()
                .map(|entry| (entry.id, format!("{:?}", entry.issue_code).to_lowercase()))
                .collect()
        })
        .unwrap_or_default()
}

fn extension_is_enabled(
    id: &str,
    session_enabled: Option<&std::collections::BTreeSet<String>>,
    enablement: &std::collections::BTreeMap<String, ExtensionEnablement>,
) -> bool {
    let registry_enabled = enablement
        .get(id)
        .copied()
        .unwrap_or(ExtensionEnablement::Disabled)
        .is_enabled();
    session_enabled
        .map(|set| set.contains(id))
        .unwrap_or(registry_enabled)
}

fn bundled_manager_items(
    session_enabled: Option<&std::collections::BTreeSet<String>>,
    enablement: &std::collections::BTreeMap<String, ExtensionEnablement>,
    audit_by_id: &std::collections::BTreeMap<String, String>,
) -> Vec<crate::ui::commands::ExtensionManagerItem> {
    let Ok(descriptors) = bundled_descriptors() else {
        return Vec::new();
    };
    descriptors
        .into_iter()
        .map(|descriptor| crate::ui::commands::ExtensionManagerItem {
            id: descriptor.id.clone(),
            display_name: descriptor.display_name.clone(),
            enabled: extension_is_enabled(&descriptor.id, session_enabled, enablement),
            bundled: true,
            materialization: None,
            version: descriptor.version.clone(),
            commands: descriptor.commands.iter().map(|c| c.name.clone()).collect(),
            capabilities: descriptor
                .capabilities
                .iter()
                .map(|c| c.as_str().to_owned())
                .collect(),
            audit_status: audit_by_id.get(&descriptor.id).cloned(),
        })
        .collect()
}

fn append_linked_manager_items(
    items: &mut Vec<crate::ui::commands::ExtensionManagerItem>,
    registry: &ExtensionRegistry,
    session_enabled: Option<&std::collections::BTreeSet<String>>,
    enablement: &std::collections::BTreeMap<String, ExtensionEnablement>,
    audit_by_id: &std::collections::BTreeMap<String, String>,
) {
    let Ok(linked) = registry.linked_extensions() else {
        return;
    };
    for link in linked {
        if items.iter().any(|item| item.id == link.id) {
            continue;
        }
        items.push(crate::ui::commands::ExtensionManagerItem {
            id: link.id.clone(),
            display_name: link.descriptor.display_name.clone(),
            enabled: extension_is_enabled(&link.id, session_enabled, enablement),
            bundled: false,
            materialization: Some(link.materialization.as_str().to_owned()),
            version: link.descriptor.version.clone(),
            commands: link
                .descriptor
                .commands
                .iter()
                .map(|c| c.name.clone())
                .collect(),
            capabilities: link.descriptor.capabilities.clone(),
            audit_status: audit_by_id.get(&link.id).cloned(),
        });
    }
}

fn set_extension_enabled(id: &str, enable: bool) -> Result<()> {
    let registry = ExtensionRegistry::new(EulerHome::resolve()?)?;
    if enable {
        registry.enable(id)?;
    } else {
        registry.disable(id)?;
    }
    Ok(())
}

fn remove_linked_extension(id: &str) -> Result<String> {
    let registry = ExtensionRegistry::new(EulerHome::resolve()?)?;
    if let Some(linked) = registry.linked_extension(id)? {
        match linked.materialization {
            ExtensionMaterialization::Installed => {
                registry.uninstall_installed(id)?;
                Ok("uninstalled".to_owned())
            }
            ExtensionMaterialization::Linked => {
                registry.unlink(id)?;
                Ok("unlinked".to_owned())
            }
        }
    } else {
        Err(anyhow!("extension {id} is not linked or installed"))
    }
}

struct ExtensionAddReport {
    id: String,
    steps: Vec<String>,
}

impl ExtensionAddReport {
    fn steps_text(&self) -> String {
        self.steps.join("\n")
    }
}

fn add_local_extension(path: &Path) -> Result<ExtensionAddReport> {
    let mut steps = Vec::new();
    let package = load_extension_package(path)?;
    let id = package.descriptor.id.clone();
    steps.push(format!(
        "validate · ok · {id} v{}",
        package.descriptor.version
    ));
    let registry = ExtensionRegistry::new(EulerHome::resolve()?)?;
    let linked = registry.link_package(package.clone())?;
    steps.push(format!(
        "link · {} · {}",
        linked.materialization.as_str(),
        linked.source_path.display()
    ));
    let installed = registry.install_package(package)?;
    steps.push(format!(
        "install · {} · {}",
        installed.materialization.as_str(),
        installed.source_path.display()
    ));
    match registry.audit() {
        Ok(report) => {
            let warnings: Vec<_> = report
                .entries
                .iter()
                .filter(|entry| entry.id == id)
                .filter(|entry| {
                    !matches!(entry.issue_code, euler_core::ExtensionAuditIssueCode::Ok)
                })
                .map(|entry| format!("audit · {} · {:?}", entry.id, entry.issue_code))
                .collect();
            if warnings.is_empty() {
                steps.push("audit · ok".to_owned());
            } else {
                steps.extend(warnings);
            }
        }
        Err(error) => steps.push(format!("audit · unavailable: {error}")),
    }
    registry.enable(&id)?;
    steps.push("enable · ok".to_owned());
    Ok(ExtensionAddReport { id, steps })
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
