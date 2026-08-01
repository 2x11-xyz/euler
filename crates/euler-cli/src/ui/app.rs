use self::activity::{ActivityTerminal, RunActivityProjection};
use self::code_swarm::load_code_swarm_models_startup;
use self::extension_runs::{list_extension_manager_items, ExtensionOutcome, ExtensionRunRequest};
use self::notify::NotifyEvent;
use self::queue_mutations::{
    QueueMutation, QueueMutationBoundary, QueueMutationCompletion, QueueMutationFailure,
    QueueMutationIntent, QueueMutationSuccess,
};
#[cfg(test)]
use self::resume::TuiResume;
#[cfg(test)]
use super::app_layout::{layout, string_lines};
use super::bottom_surface::{BottomOwner, BottomSurface, SurfaceEvent};
use super::commands::{CommandAction, CompactionSettings, PermissionPosture};
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
use super::status::{
    format_cost_picos, status_line_canvas, StatusSnapshot, TokenUsageSnapshot, TurnStatus,
};
use super::terminal::{self, PendingSignal, TerminalSession};
use super::theme::{ColorLevel, Theme, ThemeChoice};
#[cfg(test)]
use super::transcript::transcript_items_widget;
use super::transcript::{self, TranscriptItem, TranscriptState, TOOL_CALL_MAX_LINES};
use super::tui_decider::{
    PermissionChannels, PermissionPrompt, PermissionPromptEnvelope, PermissionReply, TuiDecider,
};
use super::visual_canvas::{
    BlockCursor, CanvasComposerSnapshot, CanvasLine, CanvasSpan, CanvasStatusSnapshot, FocusOwner,
    TextRole, VisualBlock, VisualBlockRole, VisualCanvasFrame, VisualCanvasSnapshot,
    VisualCanvasState,
};
use crate::extension_cli::{resolve_round_observer, ObserveOptions};
use crate::extension_enablement::{resolve_session_extensions_in_home, ExtensionSelection};
use crate::model_preference;
use anyhow::{anyhow, Result};
use chrono::Utc;
use crossterm::event::{self, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use euler_core::permissions::{PermissionRequest, PermissionRequestBatch};
use euler_core::{
    event_is_runtime_only, fold_session, load_extension_package, read_resume_prefix,
    resume_session_from_folded_prefix, AgentResult, AgentTask, ApprovalMode, CompactionStatus,
    EulerHome, ExtensionMaterialization, ExtensionRegistry, GrantSource, ModelTarget,
    ProjectContextBootstrap, ProvenanceWriter, ProviderRuntimeEvent, ProviderRuntimeObserver,
    QueueError, QueueLifecycleTransition, QueueMode, QueuePosition, QueuedInput,
    QueuedInputMetadata, ReasoningEffort, ScopePattern, Session, SessionError, SessionStore,
    SkillCatalogEntry, SteeringQueueSnapshot,
};
use euler_event::{EventEnvelope, EventKind};
use euler_provider::catalog::MergedModelCatalog;
use euler_sdk::Capability;
use ratatui::backend::CrosstermBackend;
#[cfg(test)]
use ratatui::layout::Rect;
#[cfg(test)]
use ratatui::widgets::Paragraph;
#[cfg(test)]
use ratatui::Frame;
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io::{self, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Trailing debounce for the post-resize history replay. Matches the
/// terminal's RESIZE_COMMIT_QUIESCENCE so there is exactly one notion of
/// "the resize has settled": long enough that even slow PTY delivery of a
/// drag's ticks coalesces into a single purge+replay, short enough to feel
/// immediate once the user lets go.
const RESIZE_REPLAY_DEBOUNCE: Duration = Duration::from_millis(450);
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SHUTDOWN_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const QUIT_ARM_WINDOW: Duration = Duration::from_secs(2);
const MIN_WORKED_DURATION: Duration = Duration::from_secs(5);
/// Working HUD braille spinner cadence (issue #27, spec v2.1 §13.3: 80-100ms).
const SPINNER_TICK_INTERVAL: Duration = Duration::from_millis(90);

fn inactive_permission_reply_sender() -> Sender<PermissionReply> {
    let (sender, receiver) = mpsc::channel();
    drop(receiver);
    sender
}
const QUIT_ARM_NOTICE: &str = "ctrl+c again to quit · session saved, /resume restores";
const MODEL_TURN_IN_FLIGHT_LABEL: &str = "turn";
const TURN_STARTING_NOTICE: &str =
    "turn is starting · input kept in the composer; submit again when steering is ready";
const QUEUE_MUTATION_SAVING_NOTICE: &str = "queue change is being saved";
const DENY_INSTRUCTION_SAVING_NOTICE: &str =
    "deny instruction is still being saved · approval stays open";

fn join_drafts(first: &str, second: &str) -> String {
    match (first.is_empty(), second.is_empty()) {
        (true, _) => second.to_owned(),
        (_, true) => first.to_owned(),
        (false, false) => format!("{first}\n{second}"),
    }
}

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

mod activity;
#[cfg(test)]
#[path = "app/chrome_test.rs"]
mod chrome;
mod code_swarm;
mod extension_runs;
mod notify;
mod queue_mutations;
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

/// Working HUD content, shared by the plain-text and styled render paths
/// (issue #27). See `AppCore::working_hud_line`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum HudLine {
    /// Unstyled one-liner (interrupted / turn-failed).
    Plain(String),
    /// Spinner glyph, phase verb, and dim suffix rendered as distinct spans.
    /// One line, always: reasoning TEXT is owned by the transcript's live
    /// card — the HUD carries only the global status (verb, turn timer, and
    /// the sole esc-to-interrupt affordance).
    Working {
        marker: &'static str,
        stalled: bool,
        verb: String,
        suffix: String,
        detail: Option<String>,
    },
}

pub struct App {
    terminal: CrosstermTerminal,
    _terminal_session: TerminalSession,
    event_loop: EventLoop,
    core: AppCore,
    pending_catalog_refresh: Option<PathBuf>,
    /// Trailing-debounce deadline for the post-resize history replay: every
    /// resize event pushes it out, so a drag settles into exactly ONE
    /// purge+replay at the final width instead of appending a fossil copy of
    /// the transcript to scrollback per width tick (issue #38; mechanism
    /// adopted from codex's resize reflow).
    resize_replay_deadline: Option<Instant>,
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
    /// `--auth-file` override from launch. In-app session resume must seed
    /// secret redaction from the SAME credential store the launch used;
    /// defaulting to the standard auth file would silently drop the
    /// override's values from redaction (secrets contract).
    pub auth_file: Option<PathBuf>,
}

pub struct ResumedAppState {
    pub events: Vec<EventEnvelope>,
    pub display_label: String,
    pub session_name: Option<String>,
    pub recovery_closure_appended: bool,
    pub warning_count: usize,
    pub events_replayed: usize,
}

pub struct AppCore {
    state: AppState,
    /// Worker-channel invariant witness (deep-review P3-e): while a turn is
    /// in flight the live `Box<Session>` exists only on the worker thread and
    /// returns exclusively through the `TurnInFlight` receiver's terminal
    /// event (`TurnDone` / `ExtensionDone` / `CompanionDone`); every
    /// worker→UI send is `let _ = tx.send(..)`, so dropping that receiver any
    /// other way silently loses the session. `handle_turn_event` sets this
    /// when it consumes a terminal event, licensing exactly one replacement
    /// of the `TurnInFlight` state; `install_state` consumes it and
    /// diagnoses any unlicensed replacement.
    in_flight_session_returned: bool,
    permission_rx: Receiver<PermissionPromptEnvelope>,
    reply_tx: Sender<PermissionReply>,
    active_permission_cancellation: Option<euler_sdk::CancellationToken>,
    /// Monotonic identity for the currently displayed permission prompt.
    /// Delayed deny-instruction completion may reply only to the generation
    /// that initiated its durable enqueue.
    permission_generation: u64,
    bottom: BottomSurface,
    status: StatusSnapshot,
    /// Last-known authenticated provider ids, refreshed whenever the session
    /// is Idle (construction, turn completion). Bottom-surface rebuilds that
    /// happen while the session is checked out onto the worker thread reuse
    /// this snapshot so the reviewer-model picker never silently shrinks to
    /// empty because of turn state.
    authenticated_providers: BTreeSet<String>,
    skill_commands: Vec<SkillCatalogEntry>,
    model_catalog: MergedModelCatalog,
    catalog_refresh_rx: Option<Receiver<Result<crate::provider_catalog::RefreshReport>>>,
    model_catalog_path: Option<PathBuf>,
    session_store: Option<SessionStore>,
    active_session_home_managed: bool,
    /// Whether the session loaded a durable user grant store; gates the
    /// `u  Allow <prefix> * always` approval option (absent = store inert).
    user_rules_enabled: bool,
    /// Actor recorded on `session.start`. Session cost includes every model
    /// actor, but only this actor owns the footer's active-context reading.
    primary_agent_id: Option<String>,
    token_usage: TokenUsageSnapshot,
    transcript: TranscriptState,
    visual_canvas: VisualCanvasState,
    /// Memoized render of the committed prefix of the in-flight streamed
    /// answer (see `visual::LiveCommittedCache`). Lets a spinner-forced
    /// repaint with no new committed content skip re-parsing the whole answer.
    live_committed_cache: visual::LiveCommittedCache,
    visual_scroll_offset: usize,
    composer_navigation_width: u16,
    last_working_elapsed_secs: Option<u64>,
    modal: Option<Modal>,
    /// The pending `/new` acknowledgment awaiting the card's answer. Set while
    /// `Modal::ProjectContextAck` owns the keyboard; consumed on the decision.
    pending_new_ack: Option<Box<euler_core::PendingAcknowledgment>>,
    approval_selection: ApprovalOption,
    /// Composer draft stashed while an approval modal owns the keyboard.
    /// The panel's instruction input must start EMPTY: a pre-existing draft
    /// (typed before the ask arrived) silently disabled the y/a/p/n hotkeys
    /// and was consumed as the deny instruction (issue #60).
    modal_stashed_draft: Option<String>,
    quit_armed: Option<Instant>,
    notice: Option<String>,
    pending_terminal_clipboard: Option<String>,
    interrupted_guidance: bool,
    in_flight_error: Option<String>,
    /// Global `ctrl+o` fold state (issue #49): one flag for every foldable
    /// history item — no per-cell targeting, no invisible nearest-to-
    /// viewport heuristic. All foldable cells expand together, and collapse
    /// together on the next `ctrl+o`.
    tool_output_expanded: bool,
    /// Last known history viewport as `(top_row, height)`, used to position
    /// search-match scrolling.
    last_history_viewport: (usize, usize),
    theme: Theme,
    theme_choice: ThemeChoice,
    theme_preference_path: Option<PathBuf>,
    show_timestamp_gutter: bool,
    editor: Box<dyn ExternalEditorRunner>,
    clipboard: Box<dyn ClipboardSink>,
    pending_runs: VecDeque<PendingRunRequest>,
    /// Saved /code-swarm reviewer model set (provider::model), session copy.
    code_swarm_models: Vec<String>,
    /// Pending user inputs, shared with the running turn's worker (issue
    /// #146). Entries explicitly tagged as steering are drained at model
    /// round boundaries into `user.message` events; ordinary follow-ups stay
    /// separate, and an interrupted steering group is preserved for explicit
    /// continuation. Pause state lives inside the queue so the worker
    /// respects queue editing and interrupts.
    queued_inputs: Arc<euler_core::SteeringQueue>,
    /// Durable enqueue/cancel operations run on this owned background
    /// boundary. Submitted drafts move into its labelled saving projection so
    /// the composer can accept the next message without claiming durability.
    queue_mutations: QueueMutationBoundary,
    /// Failed staged drafts remain process-private until the serialized batch
    /// settles, then return to their owning composer in request order.
    failed_queue_drafts: VecDeque<FailedQueueDraft>,
    /// A failed accepted enqueue restored text that must be resubmitted or
    /// explicitly cleared before orderly shutdown may discard it.
    queue_failure_recovery_required: bool,
    /// Edge-triggered `/compact` request shared with the root turn worker.
    /// The session consumes it at the next settled model-round boundary.
    compaction_request: Arc<AtomicBool>,
    /// Stable queue identity selected by the composer ledger. A row shifting
    /// or disappearing can never retarget a later cancellation.
    queued_selection: Option<String>,
    in_flight_label: Option<String>,
    /// Persona/name of the in-flight companion run, for approval panel tagging.
    in_flight_companion_name: Option<String>,
    in_flight_cancellable: bool,
    /// Whether the current model turn has durably admitted its run and opened
    /// the shared steering group. Turn launch stays asynchronous: before the
    /// worker reports this boundary, interactive input is an explicit
    /// follow-up and the event thread remains free to render or interrupt.
    model_turn_steering_ready: bool,
    /// Braille spinner animation frame (issue #27) — advanced by a tick
    /// counter, never derived from `Instant::now()` at render time.
    spinner_frame: usize,
    /// Wall-clock anchor for the last spinner tick; only read outside
    /// render, in the periodic background poll.
    spinner_last_tick: Option<Instant>,
    /// Deterministic projection of observable run activity. Event timestamps
    /// own phase/progress anchors; rendering supplies the current clock.
    activity: RunActivityProjection,
    extensions: ExtensionSelection,
    observe: ObserveOptions,
    /// Launch `--auth-file` override; consulted when an in-app resume
    /// re-seeds secret redaction (see [`AppOptions::auth_file`]).
    auth_file: Option<PathBuf>,
    turn_event_start: usize,
    stall_notified: bool,
    terminal_focused: bool,
    notifications_enabled: bool,
    pending_notifications: VecDeque<NotifyEvent>,
    /// Registry view of the extension manager items (computed with no
    /// session overlay). Listing hits disk — enablement log, link inventory,
    /// per-manifest reads — and `rebuild_bottom_surface` runs on the
    /// submit/turn-end hot path, so the listing is cached here and only
    /// invalidated by in-app extension mutations or a manager open (which
    /// also picks up out-of-band `euler extension` CLI changes).
    extension_registry_items: Option<Vec<crate::ui::commands::ExtensionManagerItem>>,
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
    /// The worker has durably admitted the initial user message and atomically
    /// opened the matching steering run/group in the shared queue.
    RunAdmitted,
    Event(EventEnvelope),
    /// Process-local, content-free control input for the live Activity HUD.
    /// Unlike `Event`, this is never inserted into transcript, provenance, or
    /// model context.
    ProviderRuntime(ProviderRuntimeEvent),
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
    Cancelled,
}

enum SelectedQueueRow {
    Selected(String),
    Refreshed,
    Empty,
}

struct FailedQueueDraft {
    content: Arc<str>,
    permission_generation: Option<u64>,
}

struct ProjectedQueueRow {
    queue_id: Option<String>,
    text: String,
    saving: bool,
}

#[derive(Default)]
struct QueueMutationDrain {
    changed: bool,
    failed: bool,
}

impl ProjectedQueueRow {
    fn durable(row: &QueuedInputMetadata) -> Self {
        Self {
            queue_id: Some(row.queue_id().to_owned()),
            text: row.content().to_owned(),
            saving: false,
        }
    }
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
    PermissionBatch(PermissionRequestBatch),
    PatchApproval(PatchApprovalModal),
    /// The project-context acknowledgment card for an in-app `/new` (ADR 0017
    /// phase 3). Its pending decision lives in `App::pending_new_ack`; this
    /// carries only the display state and which option is highlighted.
    ProjectContextAck(AckModalState),
    Help,
}

/// Display state for the in-app acknowledgment card. Default highlight is Skip
/// (the safe bias).
#[derive(Clone, Debug, Eq, PartialEq)]
struct AckModalState {
    folder_label: String,
    content_changed: bool,
    sources: Vec<String>,
    skipped_count: usize,
    compatibility_warning_count: usize,
    skill_count: usize,
    load_selected: bool,
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
            pending_catalog_refresh: None,
            resize_replay_deadline: None,
        })
    }

    /// Enter the TUI around an already-folded session, restoring its visible
    /// ledger before the first frame instead of presenting it as a fresh app.
    pub fn enter_resumed_with_options(
        session: Session<TuiDecider>,
        channels: PermissionChannels,
        options: AppOptions,
        resumed: ResumedAppState,
    ) -> Result<Self> {
        let mut app = Self::enter_with_options(session, channels, options)?;
        app.core.status.session_name = resumed.session_name;
        app.core.rebuild_transcript_from_events(&resumed.events);
        app.core
            .push_finalized_visual_item(TranscriptItem::ResumeBoundary {
                label: resumed.display_label,
                recovery_closure_appended: resumed.recovery_closure_appended,
                warning_count: resumed.warning_count,
                events_replayed: resumed.events_replayed,
            });
        Ok(app)
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
            self.start_pending_provider_catalog_refresh();
            self.flush_resize_replay()?;
        }
    }

    pub fn schedule_provider_catalog_refresh(&mut self, model_catalog_path: PathBuf) {
        if self.pending_catalog_refresh.is_some() || self.core.catalog_refresh_rx.is_some() {
            return;
        }
        let cache_dir =
            crate::provider_catalog::managed_catalog_dir_for_model_path(&model_catalog_path);
        if !crate::provider_catalog::automatic_refresh_due(&cache_dir) {
            return;
        }
        self.core.model_catalog_path = Some(model_catalog_path.clone());
        self.pending_catalog_refresh = Some(model_catalog_path);
    }

    fn start_pending_provider_catalog_refresh(&mut self) {
        let Some(model_catalog_path) = self.pending_catalog_refresh.take() else {
            return;
        };
        let cache_dir =
            crate::provider_catalog::managed_catalog_dir_for_model_path(&model_catalog_path);
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(crate::provider_catalog::refresh_managed_catalog(&cache_dir));
        });
        self.core.catalog_refresh_rx = Some(receiver);
    }

    /// Run the debounced post-resize replay once the deadline passes with no
    /// further resize events: purge euler-emitted scrollback and re-emit the
    /// transcript from the event log at the settled width (exactly one copy).
    fn flush_resize_replay(&mut self) -> Result<()> {
        let due = self
            .resize_replay_deadline
            .is_some_and(|deadline| Instant::now() >= deadline);
        if !due {
            return Ok(());
        }
        self.resize_replay_deadline = None;
        self.core.invalidate_history_cache();
        self.replay_history(true)
    }

    fn poll_background(&mut self) {
        if let Some(signal) = terminal::take_pending_signal() {
            self.event_loop.push(UiEvent::Signal(match signal {
                PendingSignal::Interrupt => TerminalSignal::Interrupt,
                PendingSignal::Terminate => TerminalSignal::Terminate,
            }));
        }
        // Three independent dirty checks — evaluated with `|` (not `||`) so
        // a `true` earlier in the chain can never short-circuit a later
        // one's bookkeeping (the spinner's tick-scheduling state in
        // particular must advance every poll or its cadence drifts).
        let background_dirty = self.core.drain_background();
        let timer_dirty = self.core.mark_working_timer_dirty();
        let spinner_dirty = self.core.advance_spinner();
        if background_dirty | timer_dirty | spinner_dirty {
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
        let mut timeout = self
            .event_loop
            .poll_timeout(Instant::now())
            .min(WORKER_POLL_INTERVAL);
        if let Some(deadline) = self.resize_replay_deadline {
            timeout = timeout.min(deadline.saturating_duration_since(Instant::now()));
        }
        timeout
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
                // Intermediate ticks are cheap: re-render the live viewport
                // at the new width and leave scrollback alone. The real
                // reconciliation is a SINGLE purge+replay at the settled
                // width, scheduled with a trailing debounce below — the
                // per-tick variants (purge every tick, or never purge) both
                // corrupted real terminals (review v3 §R2: Ghostty, iTerm2,
                // and Terminal.app all accumulated one fossil re-render per
                // width step).
                self.core.invalidate_history_cache();
                self.render_frame()?;
                self.resize_replay_deadline = Some(Instant::now() + RESIZE_REPLAY_DEBOUNCE);
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
        // Pause/cancel the worker before releasing a permission modal. A deny
        // wakes the blocked worker, which must observe shutdown state before
        // it can process the denial or advance the round.
        if !self.core.prepare_for_shutdown() {
            self.core.note_incomplete_shutdown();
            self.request_render(RedrawLevel::Partial);
            return Ok(false);
        }
        let lines = self.core.exit_recap_lines();
        // Clean clear (§5.8): drop the live band — the echoed `/quit`
        // composer row included — in native colors, so the recap below is
        // the only thing between the last transcript content and the shell
        // prompt. Best-effort: a failed clear must never block the exit.
        let _ = self.terminal.clear_live_band_for_exit();
        terminal::restore_terminal();
        print_exit_recap_lines(&lines);
        // Return through the normal run loop even with an active turn. This
        // lets App and all main-thread resources unwind instead of bypassing
        // destructors with process::exit. Advisory locks remain crash-safe
        // independently of orderly shutdown.
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
        // A replay rebuilds the whole canvas (theme switch, resize settle,
        // session load). Drop the incremental history cache so the rebuild
        // re-renders every finalized item at the current width/theme/config
        // instead of serving stale cached rows.
        self.core.invalidate_history_cache();
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
        core.theme.palette.user_rail,
    )
}

fn print_exit_recap_lines(lines: &[self::turn_recap::ExitRecapLine]) {
    let stdout_tty = io::stdout().is_terminal();
    if stdout_tty {
        // The recap is dim text on the terminal's native background — never a
        // fill band. Reset once up front so no stale attribute from the
        // session (theme background, dim, anything) can bleed into it, and
        // start at column 0 even if the cursor was left mid-row.
        let _ = write!(io::stdout(), "\x1b[0m\r");
    }
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
    auth_file: Option<PathBuf>,
    active_session_home_managed: bool,
    user_rules_enabled: bool,
    primary_agent_id: Option<String>,
    theme: Theme,
    status: StatusSnapshot,
    initial_token_usage: TokenUsageSnapshot,
    initial_context: super::commands::CommandContext,
    authenticated_providers: BTreeSet<String>,
}

/// §5.1: the session's active posture and its envelope, or the custom
/// envelope when the per-capability modes match no posture. One definition,
/// because `/status` and the picker title must never disagree about which
/// boundary is in force.
pub(super) fn permission_envelope_for(session: &Session<TuiDecider>) -> String {
    crate::ui::commands::PermissionPosture::active(|capability| session.configured_mode(capability))
        .map_or_else(
            || crate::ui::commands::CUSTOM_PERMISSION_ENVELOPE.to_owned(),
            |posture| posture.envelope().to_owned(),
        )
}

fn bootstrap_app_core(session: &Session<TuiDecider>, options: AppOptions) -> AppCoreBootstrap {
    let target = session.active_target().clone();
    let reasoning_effort = session.reasoning_effort();
    let session_id = session.session_id().to_owned();
    let user_rules_enabled = session.user_rules_enabled();
    let primary_agent_id = session_primary_agent_id(session);
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
        auth_file,
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
    // #64: detect truecolor support once at startup; every theme RGB
    // quantizes to ANSI-256 at this boundary when unsupported (e.g.
    // Terminal.app / TERM_PROGRAM=Apple_Terminal), so all render sites
    // degrade together instead of leaving 24-bit SGR for the terminal to
    // mangle.
    let theme = Theme::for_choice_with_color_level(theme_choice, ColorLevel::detect());
    let mut status = StatusSnapshot::new(target.provider.clone(), target.model.clone(), cwd);
    status.session_id = Some(session_id.clone());
    status.reasoning_effort = Some(reasoning_effort.as_str().to_owned());
    status.git_branch = detect_git_branch(&status.cwd);
    // /status visibility line (ADR 0011): only a non-default reviewer shows.
    if session.permission_reviewer() != euler_core::PermissionReviewer::User {
        status.permission_reviewer = Some(session.permission_reviewer().as_str().to_owned());
    }
    let initial_token_usage = TokenUsageSnapshot {
        context_window_tokens: context_window_tokens_for(
            &model_catalog,
            &target.provider,
            &target.model,
        ),
        ..TokenUsageSnapshot::default()
    };
    let authenticated_providers = session.providers().authenticated_provider_ids();
    let skill_commands = session.skill_catalog();
    let initial_context = command_context(
        &model_catalog,
        &target.provider,
        &target.model,
        &authenticated_providers,
        empty_command_context_parts(
            reasoning_effort,
            theme_choice,
            CompactionSettings {
                automatic: session.auto_compaction_policy().automatic,
                stubs: session.auto_compaction_policy().stubs_enabled(),
            },
            skill_commands.clone(),
        ),
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
        auth_file,
        active_session_home_managed,
        user_rules_enabled,
        primary_agent_id,
        theme,
        status,
        initial_token_usage,
        initial_context,
        authenticated_providers,
    }
}

fn session_primary_agent_id(session: &Session<TuiDecider>) -> Option<String> {
    session
        .events()
        .iter()
        .find(|event| event.kind.as_str() == EventKind::SESSION_START)
        .map(|event| event.agent.clone())
}

/// A short folder label for the acknowledgment card's title corner.
fn project_context_folder_label(root: &std::path::Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
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
        let skill_commands = boot.initial_context.skill_commands.clone();
        Self {
            state: AppState::Idle {
                session: Box::new(session),
            },
            in_flight_session_returned: false,
            permission_rx: channels.request_rx,
            reply_tx: inactive_permission_reply_sender(),
            active_permission_cancellation: None,
            permission_generation: 0,
            bottom: BottomSurface::new(boot.initial_context),
            status: boot.status,
            authenticated_providers: boot.authenticated_providers,
            skill_commands,
            model_catalog: boot.model_catalog,
            catalog_refresh_rx: None,
            model_catalog_path: None,
            session_store: boot.session_store,
            active_session_home_managed: boot.active_session_home_managed,
            user_rules_enabled: boot.user_rules_enabled,
            primary_agent_id: boot.primary_agent_id,
            token_usage: boot.initial_token_usage,
            transcript: TranscriptState::default(),
            visual_canvas: VisualCanvasState::new(vec![TranscriptItem::Banner {
                session_id: Some(boot.session_id),
            }]),
            live_committed_cache: visual::LiveCommittedCache::default(),
            visual_scroll_offset: 0,
            composer_navigation_width: 80,
            last_working_elapsed_secs: None,
            modal: None,
            pending_new_ack: None,
            approval_selection: ApprovalOption::default(),
            modal_stashed_draft: None,
            quit_armed: None,
            notice: None,
            pending_terminal_clipboard: None,
            interrupted_guidance: false,
            in_flight_error: None,
            tool_output_expanded: false,
            last_history_viewport: (0, 24),
            theme: boot.theme,
            theme_choice: boot.theme_choice,
            theme_preference_path: boot.theme_preference_path,
            show_timestamp_gutter: boot.show_timestamp_gutter,
            editor: Box::<SystemExternalEditor>::default(),
            clipboard: Box::<SystemClipboard>::default(),
            pending_runs: VecDeque::new(),
            code_swarm_models: load_code_swarm_models_startup(),
            queued_inputs: Arc::new(euler_core::SteeringQueue::default()),
            queue_mutations: QueueMutationBoundary::new(),
            failed_queue_drafts: VecDeque::new(),
            queue_failure_recovery_required: false,
            compaction_request: Arc::new(AtomicBool::new(false)),
            queued_selection: None,
            in_flight_label: None,
            in_flight_companion_name: None,
            in_flight_cancellable: false,
            model_turn_steering_ready: false,
            spinner_frame: 0,
            spinner_last_tick: None,
            activity: RunActivityProjection::default(),
            extensions: boot.extensions,
            observe: boot.observe,
            auth_file: boot.auth_file,
            turn_event_start: 0,
            stall_notified: false,
            terminal_focused: true,
            notifications_enabled: boot.notifications_enabled,
            pending_notifications: VecDeque::new(),
            extension_registry_items: None,
        }
    }

    fn rebuild_bottom_surface(&mut self) {
        self.refresh_authenticated_providers();
        self.refresh_skill_commands();
        let (extension_items, extension_slash_commands) = self.current_extension_context();
        let parts = CommandContextParts {
            current_effort: self.current_reasoning_effort(),
            current_theme: self.theme_choice,
            checkpoint_items: self.current_checkpoint_items(),
            extension_items,
            extension_slash_commands,
            skill_commands: self.skill_commands.clone(),
            code_swarm_models: self.code_swarm_models.clone(),
            compaction: self.current_compaction_settings(),
        };
        self.bottom.reset_context(command_context(
            &self.model_catalog,
            &self.status.provider,
            &self.status.model,
            &self.authenticated_providers,
            parts,
        ));
    }

    fn replace_bottom_surface_for_session(&mut self) {
        self.refresh_authenticated_providers();
        self.refresh_skill_commands();
        let (extension_items, extension_slash_commands) = self.current_extension_context();
        let parts = CommandContextParts {
            current_effort: self.current_reasoning_effort(),
            current_theme: self.theme_choice,
            checkpoint_items: self.current_checkpoint_items(),
            extension_items,
            extension_slash_commands,
            skill_commands: self.skill_commands.clone(),
            code_swarm_models: self.code_swarm_models.clone(),
            compaction: self.current_compaction_settings(),
        };
        self.bottom = BottomSurface::new(command_context(
            &self.model_catalog,
            &self.status.provider,
            &self.status.model,
            &self.authenticated_providers,
            parts,
        ));
    }

    /// Recomputes the authenticated-provider snapshot when the session is on
    /// this thread (Idle). While a turn is in flight the session lives on the
    /// worker thread, so rebuilds keep the last-known snapshot instead of
    /// degrading the reviewer-model picker to an empty list.
    fn refresh_authenticated_providers(&mut self) {
        if let AppState::Idle { session } = &self.state {
            self.authenticated_providers = session.providers().authenticated_provider_ids();
        }
    }

    fn refresh_skill_commands(&mut self) {
        if let AppState::Idle { session } = &self.state {
            self.skill_commands = session.skill_catalog();
        }
    }

    fn current_compaction_settings(&self) -> CompactionSettings {
        match &self.state {
            AppState::Idle { session } => {
                let policy = session.auto_compaction_policy();
                CompactionSettings {
                    automatic: policy.automatic,
                    stubs: policy.stubs_enabled(),
                }
            }
            AppState::Empty | AppState::TurnInFlight { .. } => self.bottom.context().compaction,
        }
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
        &mut self,
    ) -> (
        Vec<crate::ui::commands::ExtensionManagerItem>,
        Vec<crate::ui::commands::ExtensionSlashCommand>,
    ) {
        // Registry listing is cached (disk-backed, hot path). The manager
        // reports current user-scope launch consent; session/project selection
        // is resolved separately.
        let items = match &self.extension_registry_items {
            Some(items) => items.clone(),
            None => {
                let items = list_extension_manager_items();
                self.extension_registry_items = Some(items.clone());
                items
            }
        };
        let slash = crate::ui::commands::build_extension_slash_commands(&items);
        (items, slash)
    }

    /// Drops the cached registry listing so the next
    /// `current_extension_context` re-reads the extension registry.
    fn invalidate_extension_registry_items(&mut self) {
        self.extension_registry_items = None;
    }

    fn current_reasoning_effort(&self) -> ReasoningEffort {
        self.status
            .reasoning_effort
            .as_deref()
            .and_then(ReasoningEffort::parse)
            .unwrap_or_default()
    }

    fn composer_snapshot(&self) -> ComposerSnapshot<'_> {
        ComposerSnapshot::new(self.bottom.composer()).with_queued(self.queued_composer_lines())
    }

    fn queued_composer_lines(&self) -> Vec<QueuedComposerLine> {
        let snapshot = self.queued_inputs.metadata_snapshot();
        let mut rows = self.durable_queue_projection(&snapshot);
        for pending in self.queue_mutations.pending_enqueues() {
            let row = ProjectedQueueRow {
                queue_id: None,
                text: pending.content,
                saving: true,
            };
            match pending.position {
                QueuePosition::Front => rows.insert(0, row),
                QueuePosition::Back => rows.push(row),
            }
        }
        let total = rows.len();
        let selected = self
            .queued_selection
            .as_deref()
            .filter(|selected| {
                rows.iter()
                    .any(|row| row.queue_id.as_deref() == Some(*selected))
            })
            .map(str::to_owned)
            .or_else(|| rows.iter().rev().find_map(|row| row.queue_id.clone()));
        rows.into_iter()
            .enumerate()
            .map(|(index, row)| QueuedComposerLine {
                position: index + 1,
                total,
                text: row.text,
                selected: row.queue_id == selected,
                saving: row.saving,
            })
            .collect()
    }

    fn durable_queue_projection(&self, current: &SteeringQueueSnapshot) -> Vec<ProjectedQueueRow> {
        let Some(baseline) = self.queue_mutations.projection_baseline() else {
            return current
                .rows()
                .iter()
                .map(ProjectedQueueRow::durable)
                .collect();
        };
        baseline
            .iter()
            .filter_map(|baseline_row| {
                current
                    .rows()
                    .iter()
                    .find(|row| row.queue_id() == baseline_row.queue_id)
                    .map(ProjectedQueueRow::durable)
                    .or_else(|| {
                        self.queue_mutations
                            .cancellation_pending(&baseline_row.queue_id)
                            .then(|| ProjectedQueueRow {
                                queue_id: Some(baseline_row.queue_id.clone()),
                                text: baseline_row.content.clone(),
                                saving: false,
                            })
                    })
            })
            .collect()
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
        if !self.turn_in_flight() {
            let compaction_in_progress = matches!(
                &self.state,
                AppState::Idle { session } if session.compaction_in_progress()
            );
            let cancelled = self.interrupt_idle_compaction("user interrupt");
            if !compaction_in_progress {
                return CoreEffect::None;
            }
            return match cancelled {
                Ok(CompactionStatus::Applied) => self.notice_item("compaction complete".to_owned()),
                Ok(CompactionStatus::Cancelled) => {
                    self.notice_item("compaction interrupted · active canvas unchanged".to_owned())
                }
                Ok(CompactionStatus::Failed) => {
                    self.notice_item("compaction failed · active canvas unchanged".to_owned())
                }
                Ok(CompactionStatus::Unchanged) => CoreEffect::None,
                Ok(CompactionStatus::InProgress) => self.error_item(
                    "compaction interruption failed: compaction is still in progress".to_owned(),
                ),
                Err(error) => self.error_item(format!("compaction interruption failed: {error}")),
            };
        }
        let cleared = self.pending_runs.len();
        self.pending_runs.clear();
        self.compaction_request.store(false, Ordering::SeqCst);
        if cleared > 0 {
            let noun = if cleared == 1 {
                "queued activity"
            } else {
                "queued activities"
            };
            self.push_notice_item(format!("interrupt cleared {cleared} {noun}"));
        }
        if self.is_in_flight_cancellable() {
            let AppState::TurnInFlight { interrupt_flag, .. } = &self.state else {
                unreachable!("turn-in-flight state checked above");
            };
            // Pause BEFORE publishing cancellation: the queue holds this
            // boundary across steering persistence, so either a message was
            // durably absorbed first or it remains queued after this returns.
            self.queued_inputs.set_paused(true);
            interrupt_flag.store(true, Ordering::SeqCst);
            self.interrupted_guidance = true;
        } else {
            // The interrupt is dropped, not deferred. Say so for the few
            // short background operations that have no token.
            self.notice =
                Some("current activity is not cancellable; it will finish shortly".to_owned());
        }
        CoreEffect::Render
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

    /// Shutdown hygiene, publication phase: an active driver receives the
    /// same signal as Escape, then [`Self::prepare_for_shutdown`] waits
    /// boundedly for its owned session. An idle shadow compaction instead
    /// crosses the session lifecycle barrier, settling an already-finished
    /// result or terminally cancelling its canonical call before the session
    /// is dropped. Catalog refresh remains a single short call without a
    /// cancellation signal.
    pub fn cancel_in_flight_for_shutdown(&mut self) {
        self.compaction_request.store(false, Ordering::SeqCst);
        match &self.state {
            AppState::TurnInFlight { interrupt_flag, .. } => {
                // Same ordering contract as `handle_interrupt`: a worker that
                // observes the flag must also observe the pause.
                self.queued_inputs.set_paused(true);
                interrupt_flag.store(true, Ordering::SeqCst);
            }
            AppState::Idle { .. } => {
                let _ = self.cancel_idle_compaction_for_lifecycle("session shutdown");
            }
            AppState::Empty => {}
        }
    }

    fn prepare_for_shutdown(&mut self) -> bool {
        self.cancel_in_flight_for_shutdown();
        self.pending_runs.clear();
        let deadline = Instant::now() + SHUTDOWN_CLEANUP_TIMEOUT;
        if !self.await_queue_mutations_for_shutdown(deadline) {
            return false;
        }
        if self.queue_failure_recovery_required {
            if self.queue_recovery_draft_present() {
                return false;
            }
            // Emptying the restored draft is the explicit abandon action.
            self.queue_failure_recovery_required = false;
        }
        self.deny_open_modal();
        self.await_in_flight_shutdown(deadline)
    }

    fn note_incomplete_shutdown(&mut self) {
        let typed_queue_failure = self.notice.as_deref().is_some_and(|notice| {
            notice.starts_with("queue input failed:") || notice.starts_with("unqueue failed:")
        });
        if self.queue_failure_recovery_required && !typed_queue_failure {
            self.notice = Some(
                "queue input was not saved · draft restored; resubmit it or clear it before quitting"
                    .to_owned(),
            );
        } else if !typed_queue_failure {
            self.notice =
                Some("still stopping active work; quit again after cleanup completes".to_owned());
        }
    }

    fn await_queue_mutations_for_shutdown(&mut self, deadline: Instant) -> bool {
        let mut failed = false;
        loop {
            let drained = self.drain_queue_mutations_with_status();
            failed |= drained.failed;
            if !self.queue_mutations.has_pending() {
                return !failed;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(1)));
        }
    }

    fn await_in_flight_shutdown(&mut self, deadline: Instant) -> bool {
        loop {
            let event = {
                let AppState::TurnInFlight { worker_rx, .. } = &self.state else {
                    return true;
                };
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return false;
                }
                match worker_rx.recv_timeout(remaining) {
                    Ok(event) => event,
                    Err(RecvTimeoutError::Timeout) => return false,
                    // The worker and its owned session are already gone. There
                    // is nothing left for an orderly shutdown to reclaim.
                    Err(RecvTimeoutError::Disconnected) => return true,
                }
            };
            self.handle_turn_event(event);
        }
    }

    pub fn drain_background(&mut self) -> bool {
        let mut changed = self.drain_catalog_refresh();
        changed |= self.drain_queue_mutations();
        changed |= self.drain_permissions();
        changed |= self.drain_idle_compaction();
        while let Some(event) = self.next_turn_event() {
            changed = true;
            self.handle_turn_event(event);
        }
        self.check_stall_notification();
        changed
    }

    fn drain_queue_mutations(&mut self) -> bool {
        self.drain_queue_mutations_with_status().changed
    }

    fn drain_queue_mutations_with_status(&mut self) -> QueueMutationDrain {
        let mut drained = QueueMutationDrain::default();
        while let Some(completion) = self.queue_mutations.try_complete() {
            drained.failed |= self.finish_queue_mutation(completion);
            drained.changed = true;
        }
        if drained.changed && !self.queue_mutations.has_pending() {
            self.restore_failed_queue_drafts();
            self.clear_queue_mutation_notice();
        }
        drained
    }

    /// Reconcile one worker result. Returns whether the authoritative
    /// operation failed, which lets orderly shutdown refuse to discard a
    /// restored draft.
    fn finish_queue_mutation(&mut self, completion: QueueMutationCompletion) -> bool {
        match (completion.intent, completion.result) {
            (
                QueueMutationIntent::ComposerSubmit { .. },
                Ok(QueueMutationSuccess::Enqueued { queue_id }),
            ) => {
                self.queued_selection = Some(queue_id);
                self.normalize_queue_selection();
                self.clear_queue_mutation_notice();
                false
            }
            (
                QueueMutationIntent::DenyInstruction {
                    original,
                    permission_generation,
                },
                Ok(QueueMutationSuccess::Enqueued { queue_id }),
            ) => {
                self.queued_selection = Some(queue_id);
                if self.modal.is_none() || self.permission_generation != permission_generation {
                    self.normalize_queue_selection();
                    self.clear_queue_mutation_notice();
                    return false;
                }
                let current = self.bottom.composer().submit_text();
                let changed_while_saving = !current.is_empty();
                self.bottom.replace_composer_text("");
                self.reply_to_modal(PermissionReply::DenyWithInstruction(original.to_string()));
                if changed_while_saving {
                    self.append_preserved_composer(&current);
                }
                self.normalize_queue_selection();
                self.clear_queue_mutation_notice();
                false
            }
            (
                QueueMutationIntent::Recall { queue_id: _ },
                Ok(QueueMutationSuccess::Cancelled { content }),
            ) => {
                let current = self.bottom.composer().submit_text();
                self.bottom.replace_composer_text(&content);
                self.append_preserved_composer(&current);
                self.normalize_queue_selection();
                self.clear_queue_mutation_notice();
                false
            }
            (
                QueueMutationIntent::Unqueue { queue_id: _ },
                Ok(QueueMutationSuccess::Cancelled { content: _ }),
            ) => {
                self.normalize_queue_selection();
                self.clear_queue_mutation_notice();
                false
            }
            (intent, Err(error)) => {
                self.finish_queue_mutation_failure(&intent, &error);
                true
            }
            (intent, Ok(_)) => {
                self.finish_queue_mutation_failure(
                    &intent,
                    &QueueMutationFailure::UnexpectedResult,
                );
                true
            }
        }
    }

    fn finish_queue_mutation_failure(
        &mut self,
        intent: &QueueMutationIntent,
        error: &QueueMutationFailure,
    ) {
        match intent {
            QueueMutationIntent::ComposerSubmit { original } => {
                self.queue_failure_recovery_required = true;
                self.failed_queue_drafts.push_back(FailedQueueDraft {
                    content: Arc::clone(original),
                    permission_generation: None,
                });
            }
            QueueMutationIntent::DenyInstruction {
                original,
                permission_generation,
            } => {
                self.queue_failure_recovery_required = true;
                self.failed_queue_drafts.push_back(FailedQueueDraft {
                    content: Arc::clone(original),
                    permission_generation: Some(*permission_generation),
                });
            }
            QueueMutationIntent::Recall { .. } | QueueMutationIntent::Unqueue { .. } => {}
        }
        let operation = match intent {
            QueueMutationIntent::ComposerSubmit { .. }
            | QueueMutationIntent::DenyInstruction { .. } => "queue input failed",
            QueueMutationIntent::Recall { .. } | QueueMutationIntent::Unqueue { .. } => {
                "unqueue failed"
            }
        };
        let recovery = match intent {
            QueueMutationIntent::ComposerSubmit { .. }
            | QueueMutationIntent::DenyInstruction { .. } => {
                " · draft restored; resubmit it or clear it before quitting"
            }
            QueueMutationIntent::Recall { .. } | QueueMutationIntent::Unqueue { .. } => "",
        };
        self.normalize_queue_selection();
        self.notice = Some(format!("{operation}: {error}{recovery}"));
    }

    fn queue_recovery_draft_present(&self) -> bool {
        !self.failed_queue_drafts.is_empty()
            || !self.bottom.composer().submit_text().is_empty()
            || self
                .modal_stashed_draft
                .as_deref()
                .is_some_and(|draft| !draft.is_empty())
    }

    fn clear_composer_if_unchanged(&mut self, original: &str) {
        if self.bottom.composer().submit_text() == original {
            self.bottom.replace_composer_text("");
        }
    }

    fn clear_queue_mutation_notice(&mut self) {
        if self.queue_mutations.has_pending() {
            return;
        }
        let owned = self.notice.as_deref().is_some_and(|notice| {
            notice == QUEUE_MUTATION_SAVING_NOTICE
                || notice == DENY_INSTRUCTION_SAVING_NOTICE
                || notice.ends_with("that queue change is already being saved")
        });
        if owned {
            self.notice = None;
        }
    }

    fn restore_failed_queue_drafts(&mut self) {
        if self.failed_queue_drafts.is_empty() {
            return;
        }
        let mut main = Vec::new();
        let mut permission = Vec::new();
        while let Some(failed) = self.failed_queue_drafts.pop_front() {
            let belongs_to_current_permission = failed
                .permission_generation
                .is_some_and(|id| self.modal.is_some() && self.permission_generation == id);
            if belongs_to_current_permission {
                permission.push(failed.content.to_string());
            } else {
                main.push(failed.content.to_string());
            }
        }
        if !main.is_empty() {
            let failed = main.join("\n");
            if self.modal.is_some() {
                let stashed = self.modal_stashed_draft.take().unwrap_or_default();
                self.modal_stashed_draft = Some(join_drafts(&failed, &stashed));
            } else {
                let current = self.bottom.composer().submit_text();
                self.bottom
                    .replace_composer_text(&join_drafts(&failed, &current));
            }
        }
        if !permission.is_empty() {
            let failed = permission.join("\n");
            let current = self.bottom.composer().submit_text();
            self.bottom
                .replace_composer_text(&join_drafts(&failed, &current));
        }
    }

    fn append_preserved_composer(&mut self, preserved: &str) {
        if preserved.is_empty() {
            return;
        }
        let current = self.bottom.composer().submit_text();
        let combined = if current.is_empty() {
            preserved.to_owned()
        } else {
            format!("{current}\n{preserved}")
        };
        self.bottom.replace_composer_text(&combined);
    }

    fn drain_catalog_refresh(&mut self) -> bool {
        let Some(receiver) = &self.catalog_refresh_rx else {
            return false;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => {
                self.catalog_refresh_rx = None;
                self.push_notice_item(
                    "provider catalog refresh stopped · using last-known-good models".to_owned(),
                );
                return true;
            }
        };
        self.catalog_refresh_rx = None;
        match result {
            Ok(report) => self.accept_catalog_refresh(report),
            Err(error) => self.push_notice_item(format!(
                "provider catalog refresh unavailable · using last-known-good models ({error})"
            )),
        }
        true
    }

    fn accept_catalog_refresh(&mut self, report: crate::provider_catalog::RefreshReport) {
        if report.outcome.was_updated() {
            self.reload_model_catalog();
            self.push_notice_item("provider catalog updated · latest models available".to_owned());
        } else {
            self.push_notice_item("provider catalog is current".to_owned());
        }
        if !report.warnings.is_empty() {
            self.push_notice_item(format!(
                "provider catalog cache reported {} warning(s)",
                report.warnings.len()
            ));
        }
    }

    fn reload_model_catalog(&mut self) {
        let Some(path) = self.model_catalog_path.clone() else {
            return;
        };
        let load = crate::model_catalog::load_model_catalog(Some(&path));
        self.install_model_catalog(load.catalog);
        if !load.warnings.is_empty() {
            self.push_notice_item(format!(
                "provider catalog loaded with {} local warning(s)",
                load.warnings.len()
            ));
        }
    }

    fn install_model_catalog(&mut self, catalog: MergedModelCatalog) {
        self.model_catalog = catalog;
        if let AppState::Idle { session } = &mut self.state {
            session.set_model_catalog(self.model_catalog.clone());
        }
        // A worker owns the session while a turn is in flight. Keep that
        // turn's routing policy coherent; `accept_worker_session_or_continue`
        // installs the latest catalog before any queued or subsequent turn starts.
        self.token_usage.context_window_tokens = self.active_context_window_tokens();
        self.rebuild_bottom_surface();
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
        // Escape is dispatched from the topmost UI layer inward. A palette,
        // picker, search surface, or confirmation prompt gets the first key;
        // only a later Escape, once the composer owns input again, can
        // interrupt the active turn.
        if key.code == KeyCode::Esc && !matches!(self.bottom.owner(), BottomOwner::Composer) {
            return self.handle_surface_key(key);
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
            KeyCode::Char(' ') if self.bottom.is_compaction_picker() => {
                if let Some(event) = self.bottom.compaction_toggle() {
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
                if self.bottom.picker_backspace_leaves_permissions_advanced() {
                    return self.open_permissions_picker();
                }
                if self.bottom.picker_backspace_steps_back() {
                    return CoreEffect::Render;
                }
                // Issue #23: backspacing over the leading `/` with nothing
                // else typed exits the palette (same as Esc) instead of the
                // prior no-op clamp.
                if self.bottom.palette_backspace_would_exit() {
                    let event = self.bottom.cancel();
                    return self.surface_event(event);
                }
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
        let items = self.visual_canvas.finalized_items();
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
            KeyCode::Esc => Some(self.handle_interrupt()),
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
        self.bottom.edit_composer(|draft| {
            let _ = draft.insert_bracketed_paste(text);
        });
        CoreEffect::Render
    }

    fn handle_modal_input(&mut self, input: InputEvent) -> CoreEffect {
        if matches!(self.modal, Some(Modal::ProjectContextAck(_))) {
            let InputEvent::Key(key) = input else {
                return CoreEffect::None;
            };
            return self.handle_ack_modal_key(key);
        }
        let InputEvent::Key(key) = input else {
            return self.handle_modal_composer_input(input);
        };
        if matches!(self.modal, Some(Modal::PatchApproval(_))) {
            return self.handle_patch_modal_key(key);
        }
        self.handle_approval_modal_key(key)
    }

    /// The in-app acknowledgment card (`/new`) is single-keypress: `y` loads,
    /// `n`/`Esc` skips, arrows move the highlight, Enter commits it. There is
    /// no composer here (unlike the permission panel), so keys never insert
    /// text.
    fn handle_ack_modal_key(&mut self, key: KeyEvent) -> CoreEffect {
        let load_selected = match &self.modal {
            Some(Modal::ProjectContextAck(state)) => state.load_selected,
            _ => return CoreEffect::None,
        };
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.resolve_new_session_ack(false)
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => self.resolve_new_session_ack(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.resolve_new_session_ack(false)
            }
            KeyCode::Enter => self.resolve_new_session_ack(load_selected),
            KeyCode::Up | KeyCode::Down | KeyCode::Tab => {
                if let Some(Modal::ProjectContextAck(state)) = &mut self.modal {
                    state.load_selected = !state.load_selected;
                }
                CoreEffect::Render
            }
            _ => CoreEffect::None,
        }
    }

    /// Complete a `/new` from the acknowledgment card's answer. Accept writes
    /// the durable record (fail closed on write error, running without the
    /// guidance); decline is a session-only tombstone.
    fn resolve_new_session_ack(&mut self, accept: bool) -> CoreEffect {
        self.modal = None;
        self.restore_stashed_draft();
        let Some(pending) = self.pending_new_ack.take() else {
            return CoreEffect::Render;
        };
        let bootstrap = if accept {
            match pending.accept() {
                Ok(bootstrap) => bootstrap,
                Err(error) => {
                    // Fail closed: never admit without a recorded acceptance.
                    let _ = self.notice_item(format!("project guidance not loaded: {error}"));
                    pending.decline()
                }
            }
        } else {
            pending.decline()
        };
        self.finish_new_session(bootstrap)
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
        if self.queue_mutations.deny_instruction_pending() {
            return self.handle_pending_deny_instruction_key(key);
        }
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
            KeyCode::Char('p') | KeyCode::Char('P') if draft_empty && !self.modal_is_batch() => {
                let prefix = self.modal_scope_prefix().unwrap_or_default();
                self.reply_to_modal(PermissionReply::AllowProjectScope(prefix))
            }
            // `u` decides only when the panel actually offers the durable
            // user rule; otherwise it falls through and types into the
            // composer like any other character.
            KeyCode::Char('u') | KeyCode::Char('U')
                if draft_empty && self.modal_user_rule_prefix().is_some() =>
            {
                let prefix = self.modal_user_rule_prefix().unwrap_or_default();
                self.reply_to_modal(PermissionReply::AllowUserScope(prefix))
            }
            KeyCode::Char('n') | KeyCode::Char('N') if draft_empty => self.reply_deny_from_modal(),
            KeyCode::Esc => self.reply_deny_from_modal(),
            _ if modal_quit_key(&key) => {
                // App::shutdown owns the bare denial. It first publishes the
                // steering pause and cancellation flag, then releases this
                // modal so the permission-blocked worker cannot wake into a
                // still-live round.
                CoreEffect::Quit
            }
            _ => self.handle_modal_composer_key(key),
        }
    }

    fn handle_pending_deny_instruction_key(&mut self, key: KeyEvent) -> CoreEffect {
        match key.code {
            KeyCode::Enter if enter_key_intent(&key) == Some(EnterKeyIntent::InsertNewline) => {
                self.handle_modal_composer_key(key)
            }
            KeyCode::Char(ch) if text_entry_modifiers(key.modifiers) => {
                self.edit_composer_text(|draft| draft.insert_char(ch))
            }
            KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End => self.handle_modal_composer_key(key),
            _ => {
                self.notice = Some(DENY_INSTRUCTION_SAVING_NOTICE.to_owned());
                CoreEffect::Render
            }
        }
    }

    fn move_approval_selection_up(&mut self) -> CoreEffect {
        if self.modal_is_batch() {
            self.approval_selection = match self.approval_selection {
                ApprovalOption::AllowOnce => ApprovalOption::AllowOnce,
                ApprovalOption::AllowSession => ApprovalOption::AllowOnce,
                ApprovalOption::Deny => ApprovalOption::AllowSession,
                ApprovalOption::AllowProject | ApprovalOption::AllowUser => {
                    ApprovalOption::AllowOnce
                }
            };
            return CoreEffect::Render;
        }
        self.approval_selection = self
            .approval_selection
            .previous(self.modal_user_rule_prefix().is_some());
        CoreEffect::Render
    }

    fn move_approval_selection_down(&mut self) -> CoreEffect {
        if self.modal_is_batch() {
            self.approval_selection = match self.approval_selection {
                ApprovalOption::AllowOnce => ApprovalOption::AllowSession,
                ApprovalOption::AllowSession => ApprovalOption::Deny,
                ApprovalOption::Deny => ApprovalOption::Deny,
                ApprovalOption::AllowProject | ApprovalOption::AllowUser => ApprovalOption::Deny,
            };
            return CoreEffect::Render;
        }
        self.approval_selection = self
            .approval_selection
            .next(self.modal_user_rule_prefix().is_some());
        CoreEffect::Render
    }

    fn reply_to_selected_approval(&mut self) -> CoreEffect {
        if self.modal_is_batch() {
            return match self.approval_selection {
                ApprovalOption::AllowOnce => self.reply_to_modal(PermissionReply::AllowOnce),
                ApprovalOption::AllowSession => {
                    self.reply_to_modal(PermissionReply::AllowSessionScope(String::new()))
                }
                ApprovalOption::AllowProject | ApprovalOption::AllowUser | ApprovalOption::Deny => {
                    self.reply_deny_from_modal()
                }
            };
        }
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
            ApprovalOption::AllowUser => {
                // Unreachable without an offered prefix (navigation skips the
                // hidden row); an empty pattern degrades to allow-once at the
                // decider boundary rather than broadening.
                let prefix = self.modal_user_rule_prefix().unwrap_or_default();
                self.reply_to_modal(PermissionReply::AllowUserScope(prefix))
            }
            ApprovalOption::Deny => self.reply_deny_from_modal(),
        }
    }

    fn modal_scope_prefix(&self) -> Option<String> {
        if self.modal_is_batch() {
            return Some(String::new());
        }
        let request = self.modal_permission_request()?;
        patch_approval::derive_scope_prefix(request)
    }

    /// Prefix for the `u  Allow <prefix> * always` option: present only when
    /// the session has a loaded user grant store AND the ask is a simple
    /// shell command with a derivable first token.
    fn modal_user_rule_prefix(&self) -> Option<String> {
        if self.modal_is_batch() {
            return None;
        }
        if !self.user_rules_enabled {
            return None;
        }
        let request = self.modal_permission_request()?;
        patch_approval::derive_user_rule_prefix(request)
    }

    fn modal_permission_request(&self) -> Option<&PermissionRequest> {
        match &self.modal {
            Some(Modal::Permission(request)) => Some(request),
            Some(Modal::PatchApproval(modal)) => Some(&modal.request),
            None | Some(Modal::PermissionBatch(_) | Modal::ProjectContextAck(_) | Modal::Help) => {
                None
            }
        }
    }

    fn modal_is_batch(&self) -> bool {
        matches!(self.modal, Some(Modal::PermissionBatch(_)))
    }

    fn reply_deny_from_modal(&mut self) -> CoreEffect {
        let draft = self.bottom.composer().submit_text();
        if draft.trim().is_empty() {
            // Spec §13.2: empty composer is rail + dim cursor only, in every
            // state — the transcript's `denied` event line is the single
            // carrier of that guidance (#57). No composer ghost text here.
            self.reply_to_modal(PermissionReply::Deny)
        } else {
            // Front of queue. The decision event's `instruction` field is
            // audit-only — this queue entry is how the guidance reaches the
            // model: absorbed at the turn's next round boundary (steering),
            // or flushed as the next turn if the denial ended the turn.
            let content: Arc<str> = Arc::from(draft);
            self.start_queue_enqueue(
                QueuePosition::Front,
                Arc::clone(&content),
                QueueMutationIntent::DenyInstruction {
                    original: content,
                    permission_generation: self.permission_generation,
                },
            )
        }
    }

    fn handle_modal_composer_input(&mut self, input: InputEvent) -> CoreEffect {
        match input {
            InputEvent::Paste(text) => {
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
        self.queued_inputs.set_paused(false);
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
        if self.model_turn_waiting_for_admission() {
            self.notice = Some(TURN_STARTING_NOTICE.to_owned());
            return CoreEffect::Render;
        }
        let content: Arc<str> = Arc::from(prompt);
        self.start_queue_enqueue(
            QueuePosition::Back,
            Arc::clone(&content),
            QueueMutationIntent::ComposerSubmit { original: content },
        )
    }

    fn start_queue_enqueue(
        &mut self,
        position: QueuePosition,
        content: Arc<str>,
        intent: QueueMutationIntent,
    ) -> CoreEffect {
        let snapshot = self.queued_inputs.metadata_snapshot();
        let mode = if self.running_model_turn_accepts_steering() {
            QueueMode::Steering
        } else {
            QueueMode::FollowUp
        };
        let mutation = QueueMutation::Enqueue {
            mode,
            expected_run_id: snapshot.active_run().map(str::to_owned),
            position,
            content: Arc::clone(&content),
        };
        match self
            .queue_mutations
            .start(Arc::clone(&self.queued_inputs), mutation, intent)
        {
            Ok(()) => {
                self.clear_composer_if_unchanged(&content);
                self.notice = Some(QUEUE_MUTATION_SAVING_NOTICE.to_owned());
            }
            Err(error) => self.notice = Some(format!("queue input failed: {error}")),
        }
        CoreEffect::Render
    }

    fn running_model_turn_accepts_steering(&self) -> bool {
        matches!(self.state, AppState::TurnInFlight { .. })
            && self.in_flight_label.as_deref() == Some(MODEL_TURN_IN_FLIGHT_LABEL)
            && self.model_turn_steering_ready
    }

    fn model_turn_waiting_for_admission(&self) -> bool {
        matches!(self.state, AppState::TurnInFlight { .. })
            && self.in_flight_label.as_deref() == Some(MODEL_TURN_IN_FLIGHT_LABEL)
            && !self.model_turn_steering_ready
    }

    fn continue_queued_input(&mut self) -> CoreEffect {
        let AppState::Idle { session } = &self.state else {
            return CoreEffect::None;
        };
        if !session.can_accept_turn() {
            return CoreEffect::None;
        }
        let Some(input) = self.pop_next_queued_input() else {
            return CoreEffect::None;
        };
        self.queued_inputs.set_paused(false);
        self.visual_scroll_offset = 0;
        let session = self.take_idle_session();
        self.spawn_queued_turn(input, session);
        CoreEffect::Render
    }

    /// The single seam for `self.state` writes (deep-review P3-e). Replacing
    /// a `TurnInFlight` state drops the only receiver for the worker that
    /// owns the live session, so it is legal only right after
    /// `handle_turn_event` consumed that worker's terminal event
    /// (`in_flight_session_returned`). Any other replacement is a bug:
    /// loudly diagnosed here rather than silently losing the session.
    /// (`std::mem::replace` take-and-restore sites hold the state in a local
    /// and put it back through this method in the same expression, so the
    /// receiver never escapes.)
    fn install_state(&mut self, next: AppState) {
        let licensed = std::mem::take(&mut self.in_flight_session_returned);
        if matches!(self.state, AppState::TurnInFlight { .. }) && !licensed {
            debug_assert!(
                false,
                "worker-channel invariant violated: TurnInFlight replaced without consuming its terminal event"
            );
            self.push_notice_item(
                "internal error: in-flight worker state was replaced before its session returned; the previous session may be lost — please report this".to_owned(),
            );
        }
        self.state = next;
    }

    fn take_idle_session(&mut self) -> Box<Session<TuiDecider>> {
        match std::mem::replace(&mut self.state, AppState::Empty) {
            AppState::Idle { session } => session,
            state => {
                self.install_state(state);
                unreachable!("submit checked idle state")
            }
        }
    }

    /// §5.1: snapshot the session's permission envelope at the moment we give
    /// it away.
    ///
    /// This is the whole invariant. `/status` derives the envelope live while
    /// the session is here, so the cache is read in exactly one window — a
    /// turn in flight — and that window can only be entered through a handoff.
    /// Snapshotting here therefore covers every way the modes can differ from
    /// the last cached value: a posture change, a mode change, a revoked
    /// grant, a grant the previous turn's approval installed, and a wholly
    /// different session from `/new` or `/resume`. Refreshing at the sites
    /// that *change* modes instead only covers the ones anyone remembered.
    fn snapshot_permission_envelope(&mut self, session: &Session<TuiDecider>) {
        self.status.permission_envelope = Some(permission_envelope_for(session));
    }

    fn spawn_turn(&mut self, prompt: String, session: Box<Session<TuiDecider>>) {
        self.spawn_turn_inner(prompt, session, None);
    }

    fn spawn_queued_turn(&mut self, input: QueuedInput, session: Box<Session<TuiDecider>>) {
        let prompt = input.content().to_owned();
        self.spawn_turn_inner(prompt, session, Some(&input));
    }

    fn spawn_turn_inner(
        &mut self,
        mut prompt: String,
        mut session: Box<Session<TuiDecider>>,
        queued_input: Option<&QueuedInput>,
    ) {
        self.snapshot_permission_envelope(&session);
        // Mid-turn steering (issue #146): the worker drains this queue at
        // round boundaries; we keep pushing into our clone while the turn
        // is in flight. Re-wired every spawn so /new and /resume sessions
        // always steer the queue this AppCore renders.
        if let Some(input) = queued_input {
            match session
                .set_steering_queue_for_queued_input(Arc::clone(&self.queued_inputs), input)
            {
                Ok(canonical) => {
                    prompt = canonical.content().to_owned();
                    self.bottom.record_submission(&prompt);
                }
                Err(error) => {
                    self.install_state(AppState::Idle { session });
                    self.push_error_item(format!("queued turn failed: {error}"));
                    return;
                }
            }
        } else {
            if let Err(error) = session.set_steering_queue(Arc::clone(&self.queued_inputs)) {
                self.install_state(AppState::Idle { session });
                self.push_error_item(format!("turn setup failed: {error}"));
                return;
            }
        }
        session.set_compaction_request(Arc::clone(&self.compaction_request));
        let (worker_tx, worker_rx) = mpsc::channel();
        let runtime_tx = worker_tx.clone();
        session.set_provider_runtime_observer(ProviderRuntimeObserver::new(move |event| {
            let _ = runtime_tx.send(TurnEvent::ProviderRuntime(event));
        }));
        let interrupt_flag = Arc::new(AtomicBool::new(false));
        let worker_interrupt = Arc::clone(&interrupt_flag);
        std::thread::spawn(move || {
            let stream_tx = worker_tx.clone();
            let mut admission_reported = false;
            let result =
                session.run_turn_with_sink(&prompt, Arc::clone(&worker_interrupt), |event| {
                    if !admission_reported {
                        admission_reported = true;
                        let _ = stream_tx.send(TurnEvent::RunAdmitted);
                    }
                    let _ = stream_tx.send(TurnEvent::Event(event.clone()));
                });
            let outcome = match result {
                Ok(_) => TurnOutcome::Complete,
                Err(euler_core::SessionError::Cancelled) => TurnOutcome::Cancelled,
                Err(error) => TurnOutcome::Failed(error.to_string()),
            };
            let _ = worker_tx.send(TurnEvent::TurnDone { outcome, session });
        });
        // Install the worker immediately. Durable admission can wait on a
        // compact/fsync without freezing Ratatui rendering, Escape, or queue
        // input. `RunAdmitted` later opens the steering affordance on this
        // thread; until then submit retains the composer and asks the user to
        // retry instead of guessing a queue mode.
        self.install_state(AppState::TurnInFlight {
            worker_rx,
            interrupt_flag,
            started_at: Instant::now(),
        });
        self.in_flight_label = Some(MODEL_TURN_IN_FLIGHT_LABEL.to_owned());
        self.in_flight_companion_name = None;
        self.in_flight_cancellable = true;
        self.model_turn_steering_ready = false;
        self.last_working_elapsed_secs = None;
        self.activity.begin_at(Utc::now());
        self.spinner_frame = 0;
        self.spinner_last_tick = None;
        self.interrupted_guidance = false;
        self.in_flight_error = None;
        self.turn_event_start = self.transcript.events().len();
        self.stall_notified = false;
    }

    fn surface_event(&mut self, event: SurfaceEvent) -> CoreEffect {
        match event {
            SurfaceEvent::None => CoreEffect::Render,
            SurfaceEvent::Message(message) => self.notice_item(message),
            SurfaceEvent::Notice(message) => self.teach_notice(message),
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
            CommandAction::ActivateSkill { name, arguments } => {
                self.activate_skill(name, arguments)
            }
            CommandAction::SetCompactionPolicy { automatic, stubs } => {
                self.set_compaction_policy(automatic, stubs)
            }
            CommandAction::ExportSession { path } => self.export_session(path),
            CommandAction::ExtensionRun {
                id,
                command,
                input,
                raw_args,
            } => self.extension_run(id, command, input, raw_args),
            CommandAction::CompanionRun { input } => self.companion_run(input),
            CommandAction::CodeSwarmSaveModels { models, user_tier } => {
                self.code_swarm_save_models(models, user_tier)
            }
            CommandAction::CodeSwarmClear { user_tier } => self.code_swarm_clear(user_tier),
            CommandAction::ShowStatus => self.show_status(),
            CommandAction::Scrub { value } => self.scrub_current_session(value),
            CommandAction::Login { provider } => self.login_guidance(provider),
            CommandAction::Logout { provider } => self.logout_guidance(provider),
            CommandAction::SetTheme { choice } => self.set_theme(choice),
            CommandAction::SetPermissionMode { capability, mode } => {
                self.set_permission_mode(capability, mode)
            }
            CommandAction::SetPermissionPosture { posture } => self.set_permission_posture(posture),
            CommandAction::PermissionSandboxUnavailable => self.notice_item(
                "auto in workspace sandbox is not available yet; no permission policy changed"
                    .to_owned(),
            ),
            CommandAction::OpenPermissions => self.open_permissions_picker(),
            CommandAction::OpenPermissionsAdvanced => self.open_permissions_advanced_picker(),
            CommandAction::RevokeGrant {
                capability,
                pattern,
                source,
            } => self.revoke_grant(capability, pattern, source),
            CommandAction::ShowHelp { text } => self.notice_item(text),
            CommandAction::OpenResumePicker => self.open_resume_picker(),
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
            CommandAction::OpenExtensionManager => self.open_extension_manager(),
            CommandAction::ExtensionToggle { id, enable } => self.toggle_extension(id, enable),
            CommandAction::ExtensionDetails { id } => self.show_extension_details(id),
            CommandAction::ExtensionRemove { id } => self.remove_extension(id),
            CommandAction::ExtensionAdd { path } => self.add_extension(path),
        }
    }

    fn activate_skill(&mut self, name: String, arguments: Option<String>) -> CoreEffect {
        let prompt = arguments.map_or_else(
            || format!("/skill:{name}"),
            |arguments| format!("/skill:{name} {arguments}"),
        );
        if !matches!(self.state, AppState::Idle { .. }) {
            self.push_queued_input_back(prompt);
            self.queued_selection = self.queued_inputs.len().checked_sub(1);
            self.notice = None;
            return CoreEffect::Render;
        }
        self.visual_scroll_offset = 0;
        self.queued_inputs.set_paused(false);
        self.bottom.record_submission(&prompt);
        let session = self.take_idle_session();
        self.rebuild_bottom_surface();
        self.spawn_turn(prompt, session);
        CoreEffect::Render
    }

    fn toggle_timestamps(&mut self) -> CoreEffect {
        self.show_timestamp_gutter = !self.show_timestamp_gutter;
        // The timestamp gutter reflows every finalized row, so the whole
        // history render is stale — force a full rebuild. (With the
        // incremental cache, the trailing notice this emits is only an append
        // and would otherwise leave the prior rows rendered at the old gutter.)
        self.visual_canvas.invalidate_history_cache();
        if let Some(path) = self.theme_preference_path.as_deref() {
            if let Err(error) =
                model_preference::save_timestamps_preference(path, self.show_timestamp_gutter)
            {
                return self.teach_notice(format!(
                    "timestamps {}; preference not saved: {error}",
                    if self.show_timestamp_gutter {
                        "shown"
                    } else {
                        "hidden"
                    }
                ));
            }
        }
        // Faint confirmation line; also logged as a transcript notice item.
        let message = if self.show_timestamp_gutter {
            "timestamps shown".to_owned()
        } else {
            "timestamps hidden".to_owned()
        };
        self.teach_notice(message)
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
            Err(error) => self.error_item(format!("rollback failed: {error}")),
        }
    }

    fn start_new_session(&mut self) -> CoreEffect {
        if self.turn_in_flight() {
            return self.notice_item("new session waits for the active turn".to_owned());
        }
        match self.unresolved_authoritative_write_blocks_lifecycle() {
            Ok(true) => {
                return self.notice_item(
                    "new session waits for an unresolved authoritative session write".to_owned(),
                );
            }
            Err(error) => return self.error_item(format!("new session failed: {error}")),
            Ok(false) => {}
        }
        if let Err(error) = self.cancel_idle_compaction_for_lifecycle("new session") {
            return self.error_item(format!("new session failed: {error}"));
        }
        let AppState::Idle { session } = &self.state else {
            return self.notice_item("new session needs an active session".to_owned());
        };
        // Preflight the fresh session's project context BEFORE consuming the
        // current session: a workspace that no longer resolves fails the
        // /new honestly while the current session stays alive (ADR 0017: a
        // fresh session is never composed without its bootstrap).
        let folder_label = project_context_folder_label(session.workspace_root());
        let resolution = match session.prepare_fresh_project_context() {
            Ok(resolution) => resolution,
            Err(error) => return self.error_item(format!("new session failed: {error}")),
        };
        match resolution {
            euler_core::ProjectContextResolution::Resolved(bootstrap) => {
                self.finish_new_session(*bootstrap)
            }
            euler_core::ProjectContextResolution::Budget(error) => {
                self.error_item(format!("new session failed: {}", error.user_message()))
            }
            // Unacknowledged guidance: present the card, then compose the fresh
            // session from the answer (ADR 0017 decision 13). The current
            // session stays alive until the decision.
            euler_core::ProjectContextResolution::NeedsAcknowledgment(pending) => {
                let state = AckModalState {
                    folder_label,
                    content_changed: pending.content_changed(),
                    sources: pending.source_identities().to_vec(),
                    skipped_count: pending.skipped_count(),
                    compatibility_warning_count: pending.compatibility_warning_count(),
                    skill_count: pending.skill_count(),
                    load_selected: false,
                };
                self.pending_new_ack = Some(pending);
                self.modal_stashed_draft = Some(self.bottom.composer().submit_text().to_owned());
                self.bottom.replace_composer_text("");
                self.modal = Some(Modal::ProjectContextAck(state));
                CoreEffect::Render
            }
        }
    }

    /// Compose the fresh `/new` session from a resolved project-context
    /// bootstrap (either resolved directly, or from the acknowledgment card).
    fn finish_new_session(&mut self, project_context: ProjectContextBootstrap) -> CoreEffect {
        if !matches!(self.state, AppState::Idle { .. }) {
            return self.error_item("new session needs an active session".to_owned());
        }
        match self.unresolved_authoritative_write_blocks_lifecycle() {
            Ok(true) => {
                return self.notice_item(
                    "new session waits for an unresolved authoritative session write".to_owned(),
                );
            }
            Err(error) => return self.error_item(format!("new session failed: {error}")),
            Ok(false) => {}
        }
        let created = self.session_store().and_then(|store| {
            let record = store.create_session()?;
            Ok((record.id().to_owned(), record.events_path().to_path_buf()))
        });
        let (session_id, events_path) = match created {
            Ok(created) => created,
            Err(error) => return self.error_item(format!("new session failed: {error}")),
        };
        let writer = match ProvenanceWriter::new(&events_path) {
            Ok(writer) => writer,
            Err(error) => return self.error_item(format!("new session failed: {error}")),
        };
        let queued_inputs = Arc::clone(&self.queued_inputs);
        let transition = match queued_inputs.begin_lifecycle_transition() {
            Ok(transition) => transition,
            Err(error) => return self.error_item(format!("new session failed: {error}")),
        };
        if let Err(error) = self.clear_queued_inputs(&transition) {
            return self.error_item(format!("new session failed: {error}"));
        }
        let old_session = self.take_idle_session();
        let (decider, channels) = TuiDecider::new();
        let mut session =
            match old_session.into_fresh_session(session_id.clone(), decider, project_context) {
                Ok(session) => session.with_provenance(writer),
                Err((old_session, error)) => {
                    self.install_state(AppState::Idle {
                        session: old_session,
                    });
                    return self.error_item(format!("new session failed: {error}"));
                }
            };
        if let Err(error) = session
            .set_steering_queue_during_lifecycle_transition(Arc::clone(&queued_inputs), &transition)
        {
            self.install_state(AppState::Idle {
                session: Box::new(session),
            });
            transition.fail_closed();
            return self.error_item(format!(
                "new session failed while binding queued input ownership: {error}; restart Euler before submitting more input"
            ));
        }
        self.activate_fresh_session(session, channels, session_id)
    }

    fn activate_fresh_session(
        &mut self,
        session: Session<TuiDecider>,
        channels: PermissionChannels,
        session_id: String,
    ) -> CoreEffect {
        let active_target = session.active_target().clone();
        let reasoning_effort = session.reasoning_effort();
        let events = session.events().to_vec();

        self.permission_rx = channels.request_rx;
        self.reply_tx = inactive_permission_reply_sender();
        self.active_permission_cancellation = None;
        self.primary_agent_id = session_primary_agent_id(&session);
        self.install_state(AppState::Idle {
            session: Box::new(session),
        });
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
        self.tool_output_expanded = false;
        self.modal = None;
        self.quit_armed = None;
        self.last_working_elapsed_secs = None;
        self.interrupted_guidance = false;
        self.in_flight_error = None;
        self.notice = Some(format!("new session {session_id}"));
        CoreEffect::ReplayHistoryWithScrollbackPurge
    }

    fn companion_run(&mut self, input: serde_json::Value) -> CoreEffect {
        let request = match crate::companion_run::parse_agent_task_value(&input) {
            Ok(task) => CompanionRunRequest { task },
            Err(error) => return self.error_item(format!("companion run failed: {error}")),
        };
        match std::mem::replace(&mut self.state, AppState::Empty) {
            AppState::Idle { session } => {
                self.spawn_companion_run(request, session);
                CoreEffect::Render
            }
            state @ AppState::TurnInFlight { .. } => {
                self.install_state(state);
                self.pending_runs
                    .push_back(PendingRunRequest::Companion(request));
                self.notice = Some("queued companion run".to_owned());
                CoreEffect::Render
            }
            AppState::Empty => {
                self.install_state(AppState::Empty);
                self.notice_item("companion run needs an active session".to_owned())
            }
        }
    }

    fn spawn_companion_run(
        &mut self,
        request: CompanionRunRequest,
        mut session: Box<Session<TuiDecider>>,
    ) {
        self.snapshot_permission_envelope(&session);
        let (worker_tx, worker_rx) = mpsc::channel();
        let worker_request = request.clone();
        let interrupt_flag = Arc::new(AtomicBool::new(false));
        let worker_cancellation =
            euler_sdk::CancellationSource::from_shared_flag(Arc::clone(&interrupt_flag)).token();
        std::thread::spawn(move || {
            let start = session.events().len();
            let result = session
                .spawn_companion_with_cancel(worker_request.task.clone(), worker_cancellation);
            let events = session.events()[start..].to_vec();
            let outcome = match result {
                Ok(summary) => CompanionOutcome::Complete(summary.result),
                Err(euler_core::SessionError::Cancelled) => CompanionOutcome::Cancelled,
                Err(error) => CompanionOutcome::Failed(error.to_string()),
            };
            let _ = worker_tx.send(TurnEvent::CompanionDone {
                request: worker_request,
                outcome,
                events,
                session,
            });
        });
        self.install_state(AppState::TurnInFlight {
            worker_rx,
            interrupt_flag,
            started_at: Instant::now(),
        });
        self.in_flight_label = Some("companion run".to_owned());
        self.in_flight_companion_name = Some(request.task.persona().to_owned());
        self.in_flight_cancellable = true;
        self.model_turn_steering_ready = false;
        self.last_working_elapsed_secs = None;
        self.activity.begin_at(Utc::now());
        self.stall_notified = false;
        self.interrupted_guidance = false;
        self.in_flight_error = None;
    }

    fn login_guidance(&mut self, provider: String) -> CoreEffect {
        self.notice_item(format!(
            "Run outside the TUI:\neuler login --provider {provider}\n\nThe picker stays offline; auth is checked when a request uses the provider."
        ))
    }

    fn logout_guidance(&mut self, provider: String) -> CoreEffect {
        self.notice_item(format!(
            "Run outside the TUI:\neuler logout --provider {provider}"
        ))
    }

    fn rebuild_transcript_from_events(&mut self, events: &[EventEnvelope]) {
        let mut transcript = TranscriptState::default();
        let mut token_usage = TokenUsageSnapshot::default();
        for event in events {
            update_token_usage(
                &mut token_usage,
                event,
                self.active_context_window_tokens(),
                self.primary_agent_id.as_deref(),
            );
            transcript.push_event(event.clone());
        }
        transcript.scroll_to_bottom();
        let mut finalized = vec![transcript::ProjectedEntry {
            item: TranscriptItem::Banner {
                session_id: self.status.session_id.clone(),
            },
            timing: None,
        }];
        // Restamp the whole rebuilt transcript from real event provenance
        // (review v2 §6) rather than the blank gutter a plain items() +
        // fresh push would produce.
        let (timed_entries, clock_seed) = transcript.timed_items();
        finalized.extend(timed_entries);
        self.transcript = transcript;
        self.token_usage = token_usage;
        self.visual_canvas = VisualCanvasState::new_with_entries(finalized, clock_seed);
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

    fn recall_selected_queued_input(&mut self) -> CoreEffect {
        let queue_id = match self.selected_queue_id_for_mutation() {
            SelectedQueueRow::Selected(queue_id) => queue_id,
            SelectedQueueRow::Refreshed => return CoreEffect::Render,
            SelectedQueueRow::Empty => return self.move_composer_up_or_history(),
        };
        if !self.bottom.composer().submit_text().is_empty() {
            return self.move_composer_up_or_history();
        }
        self.start_queue_cancel(queue_id.clone(), QueueMutationIntent::Recall { queue_id });
        CoreEffect::Render
    }

    fn unqueue_selected_input(&mut self) -> CoreEffect {
        let queue_id = match self.selected_queue_id_for_mutation() {
            SelectedQueueRow::Selected(queue_id) => queue_id,
            SelectedQueueRow::Refreshed => return CoreEffect::Render,
            SelectedQueueRow::Empty => return CoreEffect::None,
        };
        self.start_queue_cancel(queue_id.clone(), QueueMutationIntent::Unqueue { queue_id });
        CoreEffect::Render
    }

    fn start_queue_cancel(&mut self, queue_id: String, intent: QueueMutationIntent) {
        let mutation = QueueMutation::Cancel { queue_id };
        match self
            .queue_mutations
            .start(Arc::clone(&self.queued_inputs), mutation, intent)
        {
            Ok(()) => self.notice = Some(QUEUE_MUTATION_SAVING_NOTICE.to_owned()),
            Err(error) => self.notice = Some(format!("unqueue failed: {error}")),
        }
    }

    /// Resolve the visible selection to one exact durable row identity. If a
    /// previously selected row disappeared, refresh the display selection but
    /// make this keypress a no-op rather than retargeting its successor.
    fn selected_queue_id_for_mutation(&mut self) -> SelectedQueueRow {
        let snapshot = self.queued_inputs.metadata_snapshot();
        if let Some(selected) = &self.queued_selection {
            if snapshot.rows().iter().any(|row| row.queue_id() == selected) {
                return SelectedQueueRow::Selected(selected.clone());
            }
            self.queued_selection = snapshot.rows().last().map(|row| row.queue_id().to_owned());
            return SelectedQueueRow::Refreshed;
        }
        let Some(selected) = snapshot.rows().last().map(|row| row.queue_id().to_owned()) else {
            return SelectedQueueRow::Empty;
        };
        self.queued_selection = Some(selected.clone());
        SelectedQueueRow::Selected(selected)
    }

    fn can_move_queued_selection(&self) -> bool {
        self.bottom.composer().submit_text().is_empty()
            && self.queued_inputs.metadata_snapshot().rows().len() > 1
    }

    fn move_queued_selection(&mut self, delta: isize) -> CoreEffect {
        let snapshot = self.queued_inputs.metadata_snapshot();
        let rows = snapshot.rows();
        let Some(last) = rows.len().checked_sub(1) else {
            return CoreEffect::None;
        };
        let index = self
            .queued_selection
            .as_deref()
            .and_then(|selected| rows.iter().position(|row| row.queue_id() == selected))
            .unwrap_or(last);
        let next = index.saturating_add_signed(delta).min(last);
        self.queued_selection = Some(rows[next].queue_id().to_owned());
        CoreEffect::Render
    }

    fn pop_next_queued_input(&mut self) -> Option<QueuedInput> {
        let prompt = self.queued_inputs.reserve_front_for_dispatch();
        self.normalize_queue_selection();
        prompt
    }

    fn normalize_queue_selection(&mut self) {
        let snapshot = self.queued_inputs.metadata_snapshot();
        if self
            .queued_selection
            .as_ref()
            .is_some_and(|selected| snapshot.rows().iter().any(|row| row.queue_id() == selected))
        {
            return;
        }
        self.queued_selection = snapshot.rows().last().map(|row| row.queue_id().to_owned());
    }

    fn clear_queued_inputs(
        &mut self,
        transition: &QueueLifecycleTransition<'_>,
    ) -> Result<(), QueueError> {
        transition.clear()?;
        self.queued_selection = None;
        self.queued_inputs.set_paused(false);
        Ok(())
    }

    fn unresolved_authoritative_write_blocks_lifecycle(&mut self) -> Result<bool, SessionError> {
        // A command accepted by the UI worker may not have entered the core
        // queue yet. Treat its staged request as part of the same lifecycle
        // fence so `/new` or `/resume` cannot rebind the queue first and let
        // that command append to the replacement session.
        let queue_blocked = self.queue_mutations.has_pending()
            || self.queued_inputs.has_unresolved_authoritative_write();
        let session_blocked = match &mut self.state {
            AppState::Idle { session } => session.has_unresolved_authoritative_write()?,
            AppState::TurnInFlight { .. } => true,
            AppState::Empty => false,
        };
        Ok(queue_blocked || session_blocked)
    }

    fn edit_palette(&mut self, edit: impl FnOnce(&mut BottomSurface)) -> CoreEffect {
        edit(&mut self.bottom);
        CoreEffect::Render
    }

    /// Issue #49: `ctrl+o` is a single global expand/collapse toggle, not a
    /// per-cell targeting gesture. Per-cell "nearest to viewport center"
    /// targeting had no honest input method once the mouse click path (#29)
    /// was removed — it was an invisible heuristic with no visible affordance
    /// to aim it. A global toggle is simple and predictable: one keystroke,
    /// every foldable cell in the transcript expands or collapses together;
    /// native scrollback and `ctrl+f` remain the navigation tools.
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
        self.tool_output_expanded = !self.tool_output_expanded;
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
        if self
            .active_permission_cancellation
            .as_ref()
            .is_some_and(euler_sdk::CancellationToken::is_cancelled)
        {
            self.dismiss_cancelled_permission_modal();
            changed = true;
        }
        while self.modal.is_none() {
            changed |= self.drain_turn_events();
            match self.permission_rx.try_recv() {
                Ok(envelope) if envelope.cancellation.is_cancelled() => {
                    changed = true;
                }
                Ok(envelope) => {
                    self.drain_turn_events();
                    if envelope.cancellation.is_cancelled() {
                        changed = true;
                    } else {
                        self.open_permission_envelope(envelope);
                        self.queue_notification(NotifyEvent::ApprovalNeeded);
                        changed = true;
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        changed
    }

    /// Open the approval modal for a request. The panel's instruction input
    /// starts EMPTY: any in-progress composer draft is stashed (and restored
    /// after the decision) so the y/a/p/n hotkeys stay live and a stale
    /// draft can never be consumed as the deny instruction (issue #60).
    fn open_permission_envelope(&mut self, envelope: PermissionPromptEnvelope) {
        self.reply_tx = envelope.reply_tx;
        self.active_permission_cancellation = Some(envelope.cancellation);
        self.open_permission_modal(envelope.prompt);
    }

    fn open_permission_modal(&mut self, prompt: impl Into<PermissionPrompt>) {
        self.permission_generation = self
            .permission_generation
            .checked_add(1)
            .expect("permission prompt identity exhausted");
        self.approval_selection = ApprovalOption::default();
        let draft = self.bottom.composer().submit_text();
        if !draft.is_empty() {
            self.modal_stashed_draft = Some(draft);
            self.bottom.replace_composer_text("");
        }
        self.modal = Some(self.modal_for_prompt(prompt.into()));
    }

    fn modal_for_prompt(&self, prompt: PermissionPrompt) -> Modal {
        match prompt {
            PermissionPrompt::Request(request) => self.modal_for_request(request),
            PermissionPrompt::Batch(batch) => Modal::PermissionBatch(batch),
        }
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

    /// Advance the working HUD's spinner animation frame (issue #27). The
    /// rendered glyph is a pure function of `spinner_frame` — a tick
    /// counter, not `Instant::now()` read at render time — so it stays
    /// testable without wall-clock assertions; only this scheduling check
    /// (called from the periodic background poll, never from render) reads
    /// the clock, exactly like the neighboring elapsed-seconds timer.
    fn advance_spinner(&mut self) -> bool {
        self.advance_spinner_at(Instant::now())
    }

    /// `now` is injected (rather than read internally) so tests can drive
    /// the tick-scheduling boundary deterministically, without sleeping —
    /// e.g. `Instant::now() - Duration::from_millis(100)` as the "last
    /// tick" to force the next call to fire.
    fn advance_spinner_at(&mut self, now: Instant) -> bool {
        if !self.turn_in_flight() {
            let was_animating = self.spinner_frame != 0 || self.spinner_last_tick.is_some();
            self.spinner_frame = 0;
            self.spinner_last_tick = None;
            return was_animating;
        }
        match self.spinner_last_tick {
            None => {
                self.spinner_last_tick = Some(now);
                false
            }
            Some(last) if now.duration_since(last) >= SPINNER_TICK_INTERVAL => {
                self.spinner_frame = self.spinner_frame.wrapping_add(1);
                self.spinner_last_tick = Some(now);
                true
            }
            Some(_) => false,
        }
    }

    fn reply_to_modal(&mut self, reply: PermissionReply) -> CoreEffect {
        self.modal = None;
        self.active_permission_cancellation = None;
        self.approval_selection = ApprovalOption::default();
        let _ = self.reply_tx.send(reply);
        self.restore_stashed_draft();
        CoreEffect::Render
    }

    /// Put back the composer draft that was in progress when the approval
    /// modal opened. The instruction typed INSIDE the panel (if any) has
    /// already been consumed by the reply path; the user's pre-ask draft
    /// returns untouched.
    fn restore_stashed_draft(&mut self) {
        if let Some(draft) = self.modal_stashed_draft.take() {
            self.bottom.replace_composer_text(&draft);
        }
    }

    fn deny_open_modal(&mut self) {
        if self.modal.take().is_some() {
            self.active_permission_cancellation = None;
            self.approval_selection = ApprovalOption::default();
            let _ = self.reply_tx.send(PermissionReply::Deny);
            self.restore_stashed_draft();
        }
    }

    fn dismiss_cancelled_permission_modal(&mut self) {
        if self.active_permission_cancellation.take().is_none() {
            return;
        }
        // Cancellation consumes no instruction. Preserve text typed in the
        // modal (including a failed staged denial restored just before this
        // poll) alongside the draft that was stashed when the ask opened.
        let cancelled_draft = if self.modal.is_some() {
            let draft = self.bottom.composer().submit_text().to_owned();
            self.bottom.replace_composer_text("");
            draft
        } else {
            String::new()
        };
        self.modal = None;
        self.approval_selection = ApprovalOption::default();
        // Cancellation is a distinct gate outcome, not a denial. The prompt's
        // one-shot receiver is already gone (or will be dropped immediately).
        self.restore_stashed_draft();
        if !cancelled_draft.is_empty() {
            let stashed = self.bottom.composer().submit_text();
            self.bottom
                .replace_composer_text(&join_drafts(&cancelled_draft, &stashed));
        }
    }

    /// Muted, non-error informational line (review v2 §3/§6/§14.4, #53) — no
    /// glyph, no "ui:" source prefix, indented to the content column.
    /// Consecutive `Notice` items stack directly without a separating blank
    /// line (the renderer special-cases this run). Used for every neutral
    /// confirmation/refusal: state guards ("waits for the active turn"),
    /// setting confirmations (/theme, /model set, /effort, /status, /usage,
    /// /compact, permission changes, extension toggles, timestamps toggle,
    /// code-swarm save/config lines, resume refusal) — none of these are
    /// failures, so none should read as one. Real failures go through
    /// `error_item` instead.
    fn notice_item(&mut self, message: String) -> CoreEffect {
        self.push_notice_item(message);
        CoreEffect::Render
    }

    fn push_notice_item(&mut self, message: String) {
        self.push_finalized_visual_item(TranscriptItem::Notice(message));
    }

    /// Alias kept for call sites that read more naturally as "teaching" the
    /// user something (disabled-extension guidance, etc.) — identical
    /// rendering to `notice_item`.
    fn teach_notice(&mut self, message: String) -> CoreEffect {
        self.notice_item(message)
    }

    /// Red, `✗`-anchored failure line (review v2 §3, #53) — reserved for
    /// genuine failures: an operation was attempted and an error came back.
    /// Never use this for state guards or confirmations.
    fn error_item(&mut self, message: String) -> CoreEffect {
        self.push_error_item(message);
        CoreEffect::Render
    }

    fn push_error_item(&mut self, message: String) {
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

    /// Working HUD content (issue #27), shared by the plain-text legacy
    /// path (`live_status_line`) and the real styled render path
    /// (`app::visual::push_visual_activity_block`). The interrupted/failed
    /// cases are unstyled one-liners; the working case carries the spinner,
    /// phase verb, and dim suffix as separate pieces so the real path can
    /// color them independently (gold spinner, dim elapsed/hint).
    fn working_hud_line(&self) -> Option<HudLine> {
        self.working_hud_line_at(Utc::now())
    }

    fn working_hud_line_at(&self, now: chrono::DateTime<Utc>) -> Option<HudLine> {
        if matches!(
            self.modal,
            Some(
                Modal::Permission(_)
                    | Modal::PermissionBatch(_)
                    | Modal::PatchApproval(_)
                    | Modal::ProjectContextAck(_)
            )
        ) {
            return None;
        }
        let interrupt = super::glyphs::interrupt();
        if self.interrupted_guidance {
            return Some(HudLine::Plain(format!(
                "{interrupt} interrupted — tell euler what to do differently"
            )));
        }
        if self.in_flight_error.is_some() {
            return Some(HudLine::Plain(format!(
                "{interrupt} turn failed — waiting for cleanup"
            )));
        }
        let AppState::TurnInFlight { started_at, .. } = &self.state else {
            return None;
        };
        let spinner = super::glyphs::glyph_set().spinner(self.spinner_frame);
        let label = self.in_flight_label.as_deref().unwrap_or("turn");
        if !self.is_in_flight_cancellable() {
            return Some(HudLine::Working {
                marker: spinner,
                stalled: false,
                verb: format!("running {label}"),
                suffix: format!(
                    " · {} · not cancellable",
                    format_live_elapsed(started_at.elapsed())
                ),
                detail: None,
            });
        }
        if label != MODEL_TURN_IN_FLIGHT_LABEL {
            return Some(HudLine::Working {
                marker: spinner,
                stalled: false,
                verb: format!("working {label}"),
                suffix: format!(
                    " · {} · esc to interrupt",
                    format_live_elapsed(started_at.elapsed())
                ),
                detail: None,
            });
        }
        let snapshot = self.activity.snapshot_at(now);
        let stalled = snapshot.stalled;
        let marker = if stalled {
            super::glyphs::interrupt()
        } else {
            spinner
        };
        let verb = if snapshot.phase == activity::ActivityPhase::Idle {
            "working".to_owned()
        } else {
            snapshot.verb()
        };
        Some(HudLine::Working {
            marker,
            stalled,
            verb,
            suffix: format!(
                " · {} · esc to interrupt",
                activity::format_age(snapshot.phase_age)
            ),
            detail: snapshot.detail(),
        })
    }

    /// Plain-text flattening of `working_hud_line`, kept for the legacy
    /// `#[cfg(test)]` Frame-based render scaffolding (render_tests_support_test.rs)
    /// that predates the visual-canvas renderer and does not carry styled spans.
    #[cfg(test)]
    fn live_status_line(&self) -> Option<String> {
        Some(match self.working_hud_line()? {
            HudLine::Plain(text) => text,
            HudLine::Working {
                marker,
                verb,
                suffix,
                ..
            } => format!("{marker} {verb}{suffix}"),
        })
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
        match &self.modal {
            Some(Modal::Permission(request)) => {
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
                    user_rule_prefix: self.modal_user_rule_prefix(),
                    companion_name: self.in_flight_companion_name.clone(),
                })
            }
            Some(Modal::PermissionBatch(batch)) => Some(TranscriptItem::PermissionBatchAsk {
                operation: batch.operation().to_owned(),
                capabilities: batch
                    .capabilities()
                    .map(|capability| capability.as_str().to_owned())
                    .collect(),
                selected_option: self.approval_selection,
            }),
            Some(Modal::ProjectContextAck(state)) => Some(TranscriptItem::ProjectContextAck {
                folder_label: state.folder_label.clone(),
                content_changed: state.content_changed,
                sources: state.sources.clone(),
                skipped_count: state.skipped_count,
                compatibility_warning_count: state.compatibility_warning_count,
                skill_count: state.skill_count,
                load_selected: state.load_selected,
            }),
            None | Some(Modal::PatchApproval(_)) | Some(Modal::Help) => None,
        }
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
        // Turn-boundary recency touch: never project the event log here —
        // this runs on the UI thread after every turn (see
        // `SessionStore::touch_session_updated_at`).
        self.session_store()?.touch_session_updated_at(session_id)?;
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
    compaction: CompactionSettings,
    skill_commands: Vec<SkillCatalogEntry>,
) -> CommandContextParts {
    CommandContextParts {
        current_effort,
        current_theme,
        checkpoint_items: Vec::new(),
        extension_items: Vec::new(),
        extension_slash_commands: Vec::new(),
        skill_commands,
        code_swarm_models: Vec::new(),
        compaction,
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
    lines.push(format!(
        "  cost:      {}",
        usage_cost_text(
            tokens.session_cost_picos,
            tokens.priced_calls,
            tokens.unpriced_calls
        )
    ));
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
    let mut lines = vec![format!(
        "usage · session totals · {}",
        usage_cost_text(
            live.session_cost_picos,
            live.priced_calls,
            live.unpriced_calls
        )
    )];
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

fn usage_cost_text(picos: u128, priced_calls: u64, unpriced_calls: u64) -> String {
    match (priced_calls, unpriced_calls) {
        (0, 0) => "unavailable".to_owned(),
        (0, _) => format!("$? ({unpriced_calls} unpriced call(s))"),
        (_, 0) => format_cost_picos(picos, 6),
        (_, _) => format!(
            "{}+ ({unpriced_calls} unpriced call(s))",
            format_cost_picos(picos, 6)
        ),
    }
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
