use self::code_swarm::load_code_swarm_models_startup;
use self::extension_runs::{list_extension_manager_items, ExtensionOutcome, ExtensionRunRequest};
use self::notify::{NotifyEvent, STALL_THRESHOLD};
#[cfg(test)]
use self::resume::TuiResume;
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
use super::patch_approval::{self, ApprovalOption, PatchApprovalModal, PatchPreview};
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
mod code_swarm;
mod extension_runs;
mod notify;
#[cfg(test)]
#[path = "app/render_tests_support_test.rs"]
mod render_tests_support;
mod resume;
mod session_commands;
mod support;
mod turn_events;
mod turn_recap;
mod visual;

#[cfg(test)]
use self::visual::ratatui_lines_to_canvas;

use self::support::{
    command_context, context_window_tokens_for, detect_git_branch, is_copy_key, merge_effects,
    read_terminal_event, session_resume_label, session_root_status_path, update_token_usage,
    CommandContextParts,
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
    approval_selection: ApprovalOption,
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
    /// Saved /code-swarm reviewer model set (provider::model), session copy.
    code_swarm_models: Vec<String>,
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

#[derive(Clone, Debug, Eq, PartialEq)]
enum TurnOutcome {
    Complete,
    Cancelled,
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CompanionOutcome {
    Complete(AgentResult),
    Failed(String),
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
                // No replay, no scrollback purge: rows already in native
                // scrollback stay untouched (re-purging duplicated them in
                // 3J-ignoring terminals and destroyed them in honoring ones —
                // the P1 audit finding). The canvas re-renders at the new
                // width and the terminal remaps its committed boundary by
                // item identity (commit_scrolled_history width branch).
                self.core.invalidate_history_cache();
                self.render_frame()?;
                return Ok(false);
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
        // Committed rows are physically in native scrollback now: freeze the
        // covered items against merges/removals (visual_canvas boundary).
        self.core
            .set_committed_history_items(self.terminal.committed_history_items());
        Ok(())
    }

    fn replay_history(&mut self, purge_scrollback: bool) -> Result<()> {
        // A replay clears and rewrites the whole canvas. Guard it with DEC
        // 2026 synchronized updates so supporting terminals paint one atomic
        // frame instead of a visible blank-then-refill sweep; the guard must
        // close even when the replay fails.
        self.terminal.begin_synchronized_update()?;
        self.core.reset_committed_history_items();
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
    initial_token_usage: TokenUsageSnapshot,
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
    // v2 Warm Spine default: spine only; /timestamps opts the gutter in.
    let show_timestamp_gutter = show_timestamp_gutter.unwrap_or(false);
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
    status.git_branch = detect_git_branch(&status.cwd);
    let initial_token_usage = TokenUsageSnapshot {
        context_window_tokens: context_window_tokens_for(
            &model_catalog,
            &target.provider,
            &target.model,
        ),
        ..TokenUsageSnapshot::default()
    };
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
        initial_token_usage,
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
            initial_token_usage,
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
            token_usage: initial_token_usage,
            transcript: TranscriptState::default(),
            visual_canvas: VisualCanvasState::new(vec![TranscriptItem::Banner {
                session_id: Some(session_id.clone()),
            }]),
            visual_scroll_offset: 0,
            composer_navigation_width: 80,
            last_working_elapsed_secs: None,
            modal: None,
            approval_selection: ApprovalOption::default(),
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
            code_swarm_models: load_code_swarm_models_startup(),
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
            code_swarm_models: self.code_swarm_models.clone(),
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
            code_swarm_models: self.code_swarm_models.clone(),
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
            KeyCode::Char(' ') if self.bottom.is_code_swarm_picker() => {
                if let Some(event) = self.bottom.code_swarm_toggle() {
                    return self.surface_event(event);
                }
                CoreEffect::None
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
            KeyCode::Up if draft_empty => self.move_approval_selection_up(),
            KeyCode::Down if draft_empty => self.move_approval_selection_down(),
            KeyCode::Enter
                if draft_empty && enter_key_intent(&key) == Some(EnterKeyIntent::Submit) =>
            {
                self.reply_to_selected_approval()
            }
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

    fn move_approval_selection_up(&mut self) -> CoreEffect {
        self.approval_selection = self.approval_selection.previous();
        CoreEffect::Render
    }

    fn move_approval_selection_down(&mut self) -> CoreEffect {
        self.approval_selection = self.approval_selection.next();
        CoreEffect::Render
    }

    fn reply_to_selected_approval(&mut self) -> CoreEffect {
        match self.approval_selection {
            ApprovalOption::AllowOnce => self.reply_to_modal(PermissionReply::AllowOnce),
            ApprovalOption::AllowSession => {
                let prefix = self.modal_scope_prefix().unwrap_or_default();
                self.reply_to_modal(PermissionReply::AllowSessionScope(prefix))
            }
            ApprovalOption::AllowProject => {
                let prefix = self.modal_scope_prefix().unwrap_or_default();
                self.reply_to_modal(PermissionReply::AllowProjectScope(prefix))
            }
            ApprovalOption::Deny => self.reply_deny_from_modal(),
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
            CommandAction::ExtensionRun {
                id,
                command,
                input,
                raw_args,
            } => self.extension_run(id, command, input, raw_args),
            CommandAction::CompanionRun { input } => self.companion_run(input),
            CommandAction::CodeSwarmSaveModels { models } => self.code_swarm_save_models(models),
            CommandAction::CodeSwarmReview { prompt, personas } => {
                self.code_swarm_review(prompt, personas)
            }
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
        self.status.git_branch = detect_git_branch(&self.status.cwd);
        self.active_session_home_managed = true;
        self.replace_bottom_surface_for_session();
        self.rebuild_transcript_from_events(&events);
        self.visual_scroll_offset = 0;
        self.token_usage.context_window_tokens = self.active_context_window_tokens();
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
                    self.approval_selection = ApprovalOption::default();
                    self.modal = Some(self.modal_for_request(request));
                    self.queue_notification(NotifyEvent::ApprovalNeeded);
                    changed = true;
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
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

    fn active_context_window_tokens(&self) -> Option<u64> {
        context_window_tokens_for(
            &self.model_catalog,
            &self.status.provider,
            &self.status.model,
        )
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
        self.approval_selection = ApprovalOption::default();
        let _ = self.reply_tx.send(reply);
        CoreEffect::Render
    }

    fn deny_open_modal(&mut self) {
        if self.modal.take().is_some() {
            self.approval_selection = ApprovalOption::default();
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
        let interrupt = super::glyphs::interrupt();
        if self.interrupted_guidance {
            return Some(format!(
                "{interrupt} interrupted — tell euler what to do differently"
            ));
        }
        if self.in_flight_error.is_some() {
            return Some(format!("{interrupt} turn failed — waiting for cleanup"));
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
        let scope_prefix = patch_approval::derive_scope_prefix(request);
        Some(TranscriptItem::PermissionAsk {
            capability: request.capability.as_str().to_owned(),
            reason: request.reason.clone(),
            command: self
                .shell_command_for_permission(request)
                .or_else(|| request.command.clone()),
            prior_count: self.prior_permission_count(request, scope_prefix.as_deref()),
            selected_option: self.approval_selection,
            scope_prefix,
            companion_name: self.in_flight_companion_name.clone(),
        })
    }

    fn prior_permission_count(
        &self,
        request: &PermissionRequest,
        scope_prefix: Option<&str>,
    ) -> usize {
        transcript::prior_permission_allow_count(
            self.transcript.events(),
            request.capability.as_str(),
            scope_prefix,
        )
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
        code_swarm_models: Vec::new(),
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
