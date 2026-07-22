use super::command::{ExecArgs, ResumeLaunch, RunArgs};
use super::extension_run::{execute_live_extension_run, wire_code_swarm};
use super::permission::CliDecider;
use super::providers::{
    invocation_target, load_known_theme_preference, load_notifications_preference,
    load_timestamps_preference, resume_provider_set_with_custom, tui_provider_set,
};
use super::terminal::{
    decide_interactive_launch, euler_no_tty_env, terminal_width, InteractiveLaunch, TuiLaunchIntent,
};
use anyhow::{anyhow, Result};
use euler_core::{
    fold_session, read_resume_prefix, resume_session_from_folded_prefix, CompactionTier,
    ModelTarget, PermissionDecider, ProvenanceWriter, ReasoningEffort, Session, SessionConfig,
    SessionKind,
};
use euler_provider::ProviderSet;
use std::io::{self, IsTerminal, Read, Write};
use std::path::Path;

use crate::auth_validation::validate_provider_auth;
use crate::companion_run::execute_headless_companion_run;
use crate::extension_cli::resolve_round_observer;
use crate::extension_enablement::resolve_session_extensions;
use crate::session_lifecycle::{
    apply_catalog_context_limit, live_session_config, resolve_resume_target, session_config,
    HomeSessionRefresh, LiveProvenance, ResumeTarget, SESSION_ID,
};
use crate::subagent::{AutoApproveTier, SubagentDecider};
use crate::ui::app::{App, AppOptions, ResumedAppState};
use crate::ui::banner;
use crate::ui::transcript::render_line_oriented;
use crate::ui::tui_decider::TuiDecider;
use crate::{diagnostics, model_preference};

pub(super) fn run_interactive_entry(
    provenance: LiveProvenance,
    mut run: RunArgs,
    default_interactive: bool,
    no_tty: bool,
) -> Result<()> {
    match decide_interactive_launch(TuiLaunchIntent {
        default_interactive,
        no_tty_arg: no_tty,
        env_no_tty: euler_no_tty_env(),
        stdin_tty: io::stdin().is_terminal(),
        stdout_tty: io::stdout().is_terminal(),
    }) {
        InteractiveLaunch::Tui => {
            apply_interactive_tui_linefeed_default(&mut run);
            run_tui(provenance, run)
        }
        InteractiveLaunch::LineOriented => run_interactive(provenance, run),
    }
}

pub(crate) fn apply_interactive_tui_linefeed_default(run: &mut RunArgs) {
    if !run.linefeed_history_insert_from_cli {
        run.linefeed_history_insert = true;
    }
}

fn run_interactive(provenance: LiveProvenance, run: RunArgs) -> Result<()> {
    let target = invocation_target(&run);
    validate_provider_auth(&target.provider, run.auth_file.as_deref(), || {
        run.provider.validate_auth()
    })?;
    let root = std::env::current_dir()?;
    let mut live_session =
        live_session_config(root, run.provider_id.clone(), run.model.clone(), provenance)?;
    live_session.config.session_kind = SessionKind::Interactive;
    apply_permission_reviewer(&mut live_session.config, &run);
    apply_catalog_context_limit(&mut live_session.config, &run.model_catalog);
    live_session.config.extensions_enabled =
        resolve_session_extensions(&live_session.config.root, &run.extensions)?;
    let observer = resolve_round_observer(&run.observe)?;
    if let Some((observer_config, _)) = &observer {
        live_session.config.round_observer = Some(observer_config.clone());
        if let Some(id) = run.observe.extension_id.as_ref() {
            live_session.config.extensions_enabled.insert(id.clone());
        }
    }
    let resolution = crate::session_lifecycle::resolve_startup_project_context(
        &live_session.config,
        run.auth_file.as_deref(),
        run.project_context,
        false,
    )?;
    let bootstrap = finalize_project_context_line(resolution, &live_session.config.root)?;
    live_session.config.project_context = Some(bootstrap);
    bind_diagnostics_for_log(&live_session.log_path);
    let providers = ProviderSet::single_named(run.provider_id.clone(), run.provider)
        .with_model_catalog(run.model_catalog.clone());
    let mut session = Session::new_with_providers(live_session.config, providers, CliDecider)
        .with_provenance(ProvenanceWriter::new(live_session.log_path)?);
    crate::session_lifecycle::seed_secret_redaction(&mut session, run.auth_file.as_deref());
    if let Some((_, extension)) = observer {
        session.set_observer_extension(extension);
    }
    wire_code_swarm(&mut session);
    run_stdin_loop(&mut session, live_session.refresh.as_ref())
}

/// A short folder label for the acknowledgment card's title corner.
fn project_context_folder_label(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}

/// Turn a pending acknowledgment into a bootstrap from the user's card answer.
/// Accept writes the durable acknowledgment; a write failure fails closed
/// (surface the remediation, run without the guidance this session). Decline
/// is session-only.
fn finalize_pending_choice(
    pending: &euler_core::PendingAcknowledgment,
    choice: crate::ui::consent_prompt::ConsentChoice,
) -> euler_core::ProjectContextBootstrap {
    use crate::ui::consent_prompt::ConsentChoice;
    match choice {
        ConsentChoice::Accept => match pending.accept() {
            Ok(bootstrap) => bootstrap,
            Err(error) => {
                eprintln!("{error}");
                pending.decline()
            }
        },
        ConsentChoice::Decline => pending.decline(),
    }
}

/// Finalize the project-context resolution for the full TUI: present the
/// bordered acknowledgment card when a decision is needed.
fn finalize_project_context_tui(
    resolution: euler_core::ProjectContextResolution,
    root: &Path,
    theme_choice: crate::ui::theme::ThemeChoice,
) -> Result<euler_core::ProjectContextBootstrap> {
    use euler_core::ProjectContextResolution as Resolution;
    match resolution {
        Resolution::Resolved(bootstrap) => Ok(*bootstrap),
        Resolution::Budget(error) => Err(anyhow!("{}", error.user_message())),
        Resolution::NeedsAcknowledgment(pending) => {
            let label = project_context_folder_label(root);
            let choice = crate::ui::consent_prompt::prompt_acknowledgment(
                &label,
                pending.content_changed(),
                pending.source_identities(),
                pending.skipped_count(),
                theme_choice,
            )?;
            Ok(finalize_pending_choice(&pending, choice))
        }
    }
}

/// Finalize for the line-oriented interactive path: a plain stdin prompt with
/// the same plain-language copy (no bordered card in line mode).
fn finalize_project_context_line(
    resolution: euler_core::ProjectContextResolution,
    root: &Path,
) -> Result<euler_core::ProjectContextBootstrap> {
    use crate::ui::consent_prompt::ConsentChoice;
    use euler_core::ProjectContextResolution as Resolution;
    match resolution {
        Resolution::Resolved(bootstrap) => Ok(*bootstrap),
        Resolution::Budget(error) => Err(anyhow!("{}", error.user_message())),
        Resolution::NeedsAcknowledgment(pending) => {
            let label = project_context_folder_label(root);
            if pending.content_changed() {
                eprintln!("The project guidance in {label} changed since you last loaded it.");
            } else {
                eprintln!(
                    "{label} ships an EULER.md with instructions for how Euler should work here."
                );
            }
            eprintln!(
                "It's guidance for the model only. It can't grant permissions or run anything."
            );
            eprint!("Load this project's guidance? It won't ask again unless it changes. [y/N] ");
            let _ = io::stderr().flush();
            let mut answer = String::new();
            let choice = if io::stdin().read_line(&mut answer).is_ok()
                && answer.trim().eq_ignore_ascii_case("y")
            {
                ConsentChoice::Accept
            } else {
                ConsentChoice::Decline
            };
            Ok(finalize_pending_choice(&pending, choice))
        }
    }
}

pub(super) fn run_tui(provenance: LiveProvenance, run: RunArgs) -> Result<()> {
    let target = invocation_target(&run);
    validate_provider_auth(&target.provider, run.auth_file.as_deref(), || {
        run.provider.validate_auth()
    })?;
    let root = std::env::current_dir()?;
    let mut live_session =
        live_session_config(root, run.provider_id.clone(), run.model.clone(), provenance)?;
    live_session.config.session_kind = SessionKind::Interactive;
    apply_permission_reviewer(&mut live_session.config, &run);
    apply_catalog_context_limit(&mut live_session.config, &run.model_catalog);
    live_session.config.extensions_enabled =
        resolve_session_extensions(&live_session.config.root, &run.extensions)?;
    let observer = resolve_round_observer(&run.observe)?;
    if let Some((observer_config, _)) = &observer {
        live_session.config.round_observer = Some(observer_config.clone());
        if let Some(id) = run.observe.extension_id.as_ref() {
            live_session.config.extensions_enabled.insert(id.clone());
        }
    }
    let preference_path = model_preference::default_model_preference_path();
    let theme_choice = load_known_theme_preference(preference_path.as_deref()).unwrap_or_default();
    // Resolve the project-context policy and, when interactive `auto` finds
    // unacknowledged guidance, present the bordered acknowledgment card BEFORE
    // the session is constructed: the decision determines the immutable
    // bootstrap the session records at session.start.
    let resolution = crate::session_lifecycle::resolve_startup_project_context(
        &live_session.config,
        run.auth_file.as_deref(),
        run.project_context,
        false,
    )?;
    let bootstrap =
        finalize_project_context_tui(resolution, &live_session.config.root, theme_choice)?;
    live_session.config.project_context = Some(bootstrap);
    bind_diagnostics_for_log(&live_session.log_path);
    let (decider, channels) = TuiDecider::new();
    let providers = tui_provider_set(run.provider_id.clone(), run.provider, &run.custom_providers)
        .with_model_catalog(run.model_catalog.clone());
    // v2 Warm Spine: timestamps are opt-in (§5.5); the anchor spine carries
    // the ledger by default.
    let show_timestamp_gutter =
        load_timestamps_preference(preference_path.as_deref()).unwrap_or(false);
    let notifications_enabled =
        load_notifications_preference(preference_path.as_deref()).unwrap_or(true);
    let mut session = Session::new_with_providers(live_session.config, providers, decider)
        .with_provenance(ProvenanceWriter::new(live_session.log_path)?);
    crate::session_lifecycle::seed_secret_redaction(&mut session, run.auth_file.as_deref());
    if let Some((_, extension)) = observer {
        session.set_observer_extension(extension);
    }
    wire_code_swarm(&mut session);
    let mut app = App::enter_with_options(
        session,
        channels,
        AppOptions {
            linefeed_history_insert: run.linefeed_history_insert,
            theme_choice,
            theme_preference_path: preference_path,
            show_timestamp_gutter: Some(show_timestamp_gutter),
            notifications_enabled: Some(notifications_enabled),
            model_catalog: Some(run.model_catalog),
            session_store: live_session
                .refresh
                .as_ref()
                .map(HomeSessionRefresh::session_store),
            extensions: run.extensions.clone(),
            observe: run.observe.clone(),
            auth_file: run.auth_file.clone(),
        },
    )?;
    if let Some(path) = crate::model_catalog::default_model_catalog_path() {
        app.schedule_provider_catalog_refresh(path);
    }
    app.run()
}

/// Run a turn while streaming each event's line-oriented rendering to stdout
/// as it is produced, flushing per event so piped or redirected `exec` output
/// is visibly incremental instead of appearing only when the turn completes
/// (issue #7). Provenance JSONL remains the canonical, detailed event stream;
/// stdout is the human-facing progress view. Each event renders standalone, so
/// the stream is strictly append-only (no retroactive coalescing to unprint).
fn run_turn_streaming<D: PermissionDecider>(session: &mut Session<D>, prompt: &str) -> Result<()> {
    let mut stdout = io::stdout();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Capture the FIRST stdout write/flush failure rather than silently
    // dropping it: a disk-full redirect must not exit 0 with lost output. Once
    // a write fails, stop trying (later writes would just re-fail).
    let mut write_error: Option<io::Error> = None;
    session.run_turn_with_sink(prompt, cancel, |event| {
        if write_error.is_some() {
            return;
        }
        let line = render_line_oriented(std::slice::from_ref(event));
        if !line.is_empty() {
            if let Err(error) = stdout
                .write_all(line.as_bytes())
                .and_then(|()| stdout.flush())
            {
                write_error = Some(error);
            }
        }
    })?;
    // A broken pipe is the normal "downstream closed" case (e.g. `| head`);
    // exit cleanly. Any other write failure is real and must surface.
    if let Some(error) = write_error {
        if error.kind() != io::ErrorKind::BrokenPipe {
            return Err(error.into());
        }
    }
    Ok(())
}

pub(super) fn run_exec(provenance: LiveProvenance, exec: ExecArgs) -> Result<()> {
    if let Some(path) = exec.resume_path {
        let prompt = read_exec_prompt(exec.prompt)?;
        return run_exec_resume(
            resolve_resume_target(path)?,
            exec.run,
            exec.auto_approve,
            prompt,
        );
    }
    let target = invocation_target(&exec.run);
    validate_provider_auth(&target.provider, exec.run.auth_file.as_deref(), || {
        exec.run.provider.validate_auth()
    })?;
    let prompt = read_exec_prompt(exec.prompt)?;
    let root = std::env::current_dir()?;
    let mut live_session = live_session_config(
        root,
        exec.run.provider_id.clone(),
        exec.run.model.clone(),
        provenance,
    )?;
    live_session.config.session_kind = SessionKind::NonInteractive;
    apply_permission_reviewer(&mut live_session.config, &exec.run);
    apply_catalog_context_limit(&mut live_session.config, &exec.run.model_catalog);
    apply_exec_config(
        &mut live_session.config,
        ExecConfigOverrides::from_run(&exec.run),
    );
    live_session.config.extensions_enabled =
        resolve_session_extensions(&live_session.config.root, &exec.run.extensions)?;
    let observer = resolve_round_observer(&exec.run.observe)?;
    if let Some((observer_config, _)) = &observer {
        live_session.config.round_observer = Some(observer_config.clone());
        if let Some(id) = exec.run.observe.extension_id.as_ref() {
            live_session.config.extensions_enabled.insert(id.clone());
        }
    }
    let resolution = crate::session_lifecycle::resolve_startup_project_context(
        &live_session.config,
        exec.run.auth_file.as_deref(),
        exec.run.project_context,
        exec.auto_approve == AutoApproveTier::TrustedLocal,
    )?;
    let bootstrap = crate::session_lifecycle::finalize_project_context_headless(resolution)?;
    live_session.config.project_context = Some(bootstrap);
    let tier = exec.auto_approve;
    let providers = ProviderSet::single_named(exec.run.provider_id.clone(), exec.run.provider)
        .with_model_catalog(exec.run.model_catalog.clone());
    let log_path = live_session.log_path.clone();
    let refresh = live_session.refresh.clone();
    bind_diagnostics_for_log(&live_session.log_path);
    let mut session = Session::new_with_providers(
        live_session.config,
        providers,
        SubagentDecider::new(exec.auto_approve),
    )
    .with_provenance(ProvenanceWriter::new(log_path)?);
    crate::session_lifecycle::seed_secret_redaction(&mut session, exec.run.auth_file.as_deref());
    if let Some((_, extension)) = observer {
        session.set_observer_extension(extension);
    }
    wire_code_swarm(&mut session);
    SubagentDecider::apply_tier(tier, &mut session);
    let turn_result = run_turn_streaming(&mut session, &prompt);
    if let Some(refresh) = refresh.as_ref() {
        if let Err(error) = refresh.refresh() {
            eprintln!("warning: failed to refresh session metadata: {error}");
        }
    }
    turn_result
}

fn run_exec_resume(
    target: ResumeTarget,
    run: RunArgs,
    auto_approve: AutoApproveTier,
    prompt: String,
) -> Result<()> {
    let overrides = ExecConfigOverrides::from_run(&run);
    let mut outcome = resume_cli_session(
        target,
        run,
        SubagentDecider::new(auto_approve),
        RelocationConsent::Headless,
        |config| {
            apply_exec_config(config, overrides);
        },
    )?;
    SubagentDecider::apply_tier(auto_approve, &mut outcome.session);
    let turn_result = run_turn_streaming(&mut outcome.session, &prompt);
    if let Some(refresh) = outcome.refresh.as_ref() {
        if let Err(error) = refresh.refresh() {
            eprintln!("warning: failed to refresh session metadata: {error}");
        }
    }
    turn_result
}

#[derive(Clone, Copy)]
struct ExecConfigOverrides {
    max_output_tokens: Option<u64>,
    max_tool_rounds: Option<usize>,
    auto_compaction: Option<CompactionTier>,
    compaction_budget_bytes: Option<usize>,
    reasoning_effort: Option<ReasoningEffort>,
}

impl ExecConfigOverrides {
    fn from_run(run: &RunArgs) -> Self {
        Self {
            max_output_tokens: run.max_output_tokens,
            max_tool_rounds: run.max_tool_rounds,
            auto_compaction: run.auto_compaction,
            compaction_budget_bytes: run.compaction_budget_bytes,
            reasoning_effort: run.reasoning_effort,
        }
    }
}

/// Route uncovered permission asks to the guardian reviewer when the
/// `--permission-reviewer guardian` flag was given (ADR 0011; default: user).
fn apply_permission_reviewer(config: &mut SessionConfig, run: &RunArgs) {
    if let Some(reviewer) = run.permission_reviewer {
        config.permission_reviewer = reviewer;
    }
}

fn apply_exec_config(config: &mut SessionConfig, overrides: ExecConfigOverrides) {
    config.max_output_tokens = overrides.max_output_tokens;
    if overrides.max_tool_rounds.is_some() {
        config.max_tool_rounds = overrides.max_tool_rounds;
    }
    if let Some(tier) = overrides.auto_compaction {
        config.auto_compaction = config
            .auto_compaction
            .with_settings(tier != CompactionTier::Off, tier == CompactionTier::Stubs);
    }
    if let Some(budget_bytes) = overrides.compaction_budget_bytes {
        config.auto_compaction.budget_bytes = budget_bytes;
    }
    if let Some(reasoning_effort) = overrides.reasoning_effort {
        config.reasoning_effort = reasoning_effort;
    }
}

fn read_exec_prompt(prompt: Option<String>) -> Result<String> {
    if let Some(prompt) = prompt {
        return non_empty_exec_prompt(prompt);
    }

    let mut stdin = io::stdin();
    if stdin.is_terminal() {
        return Err(anyhow!("exec requires a prompt argument or piped stdin"));
    }
    let mut prompt = String::new();
    stdin.read_to_string(&mut prompt)?;
    non_empty_exec_prompt(prompt)
}

pub(crate) fn non_empty_exec_prompt(prompt: String) -> Result<String> {
    let prompt = prompt.trim_end_matches(['\r', '\n']).to_owned();
    if prompt.trim().is_empty() {
        Err(anyhow!("exec requires a prompt argument or piped stdin"))
    } else {
        Ok(prompt)
    }
}

fn session_id_from_events(events: &[euler_event::EventEnvelope]) -> Option<&str> {
    events.first().map(|event| event.session.as_str())
}

pub(super) fn resume_interactive_entry(
    target: ResumeTarget,
    mut run: RunArgs,
    launch: ResumeLaunch,
    no_tty: bool,
) -> Result<()> {
    match launch {
        ResumeLaunch::Tui => {
            if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                return Err(anyhow!("tui requires terminal stdin and stdout"));
            }
            resume_tui(target, run)
        }
        ResumeLaunch::LineOriented => resume_line_oriented(target, run),
        ResumeLaunch::Auto => match decide_interactive_launch(TuiLaunchIntent {
            default_interactive: true,
            no_tty_arg: no_tty,
            env_no_tty: euler_no_tty_env(),
            stdin_tty: io::stdin().is_terminal(),
            stdout_tty: io::stdout().is_terminal(),
        }) {
            InteractiveLaunch::Tui => {
                apply_interactive_tui_linefeed_default(&mut run);
                resume_tui(target, run)
            }
            InteractiveLaunch::LineOriented => resume_line_oriented(target, run),
        },
    }
}

fn resume_line_oriented(target: ResumeTarget, run: RunArgs) -> Result<()> {
    let mut outcome = resume_cli_session(target, run, CliDecider, RelocationConsent::Line, |_| {})?;
    eprintln!(
        "resumed session {}: folded {} events, target {}/{}, recovery closure {}",
        outcome
            .session
            .events()
            .first()
            .map(|event| event.session.as_str())
            .unwrap_or(SESSION_ID),
        outcome.events_folded,
        outcome.active_target.provider,
        outcome.active_target.model,
        if outcome.recovery_closure_appended {
            "appended"
        } else {
            "not appended"
        }
    );
    run_stdin_loop(&mut outcome.session, outcome.refresh.as_ref())
}

fn resume_tui(target: ResumeTarget, run: RunArgs) -> Result<()> {
    let linefeed_history_insert = run.linefeed_history_insert;
    let model_catalog = run.model_catalog.clone();
    let extensions = run.extensions.clone();
    let observe = run.observe.clone();
    let auth_file = run.auth_file.clone();
    let (decider, channels) = TuiDecider::new();
    let preference_path = model_preference::default_model_preference_path();
    let theme_choice = load_known_theme_preference(preference_path.as_deref()).unwrap_or_default();
    // The relocation card (when needed) is shown before the resumed session is
    // built, using the resolved theme.
    let mut outcome = resume_cli_session(
        target,
        run,
        decider,
        RelocationConsent::Card(theme_choice),
        |_| {},
    )?;
    let show_timestamp_gutter =
        load_timestamps_preference(preference_path.as_deref()).unwrap_or(false);
    let notifications_enabled =
        load_notifications_preference(preference_path.as_deref()).unwrap_or(true);
    let session_id = outcome.session.session_id().to_owned();
    let events = outcome.session.events().to_vec();
    let session_store = outcome
        .refresh
        .as_ref()
        .map(HomeSessionRefresh::session_store);
    let options = AppOptions {
        linefeed_history_insert,
        theme_choice,
        theme_preference_path: preference_path,
        show_timestamp_gutter: Some(show_timestamp_gutter),
        notifications_enabled: Some(notifications_enabled),
        model_catalog: Some(model_catalog),
        session_store,
        extensions,
        observe,
        auth_file,
    };
    let mut app = App::enter_resumed_with_options(
        outcome.session,
        channels,
        options,
        ResumedAppState {
            events,
            display_label: outcome.display_label.unwrap_or(session_id),
            session_name: outcome.session_name,
            recovery_closure_appended: outcome.recovery_closure_appended,
            warning_count: outcome.warning_count,
            events_replayed: outcome.events_folded,
        },
    )?;
    if let Some(path) = crate::model_catalog::default_model_catalog_path() {
        app.schedule_provider_catalog_refresh(path);
    }
    let app_result = app.run();
    if let Some(refresh) = outcome.refresh.take() {
        if let Err(error) = refresh.refresh() {
            eprintln!("warning: failed to refresh session metadata: {error}");
        }
    }
    app_result
}

struct ResumeCliOutcome<D: PermissionDecider> {
    session: Session<D>,
    refresh: Option<HomeSessionRefresh>,
    events_folded: usize,
    active_target: ModelTarget,
    recovery_closure_appended: bool,
    warning_count: usize,
    display_label: Option<String>,
    session_name: Option<String>,
}

/// How a resume workspace relocation is consented to when the live folder does
/// not match where the session last ran.
enum RelocationConsent {
    /// Headless: accept only if `--accept-relocation` was given, else fail
    /// closed. Never prompts.
    Headless,
    /// Interactive TUI: present the bordered relocation card.
    Card(crate::ui::theme::ThemeChoice),
    /// Interactive line-oriented: a plain stdin prompt.
    Line,
}

fn decide_relocation_card(
    required: &euler_core::RelocationRequired,
    theme_choice: crate::ui::theme::ThemeChoice,
) -> Result<bool> {
    let choice = crate::ui::consent_prompt::prompt_relocation(
        required.recorded_root(),
        required.current_root(),
        required.last_active().unwrap_or("unknown"),
        theme_choice,
    )?;
    Ok(choice == crate::ui::consent_prompt::ConsentChoice::Accept)
}

fn decide_relocation_line(required: &euler_core::RelocationRequired) -> Result<bool> {
    eprintln!(
        "This session last ran in {}, but you're now in {}.",
        required.recorded_root(),
        required.current_root()
    );
    eprintln!(
        "Resuming here makes this folder the session's home. Approvals from the old folder don't \
         carry over."
    );
    eprint!("Resume here? [y/N] ");
    let _ = io::stderr().flush();
    let mut answer = String::new();
    Ok(io::stdin().read_line(&mut answer).is_ok() && answer.trim().eq_ignore_ascii_case("y"))
}

/// A same-host workspace relocation (ADR 0017 phase 3): if the live folder does
/// not match where the session last ran, obtain consent, append the durable
/// relocation event before any resumed activity, and extend the prefix.
/// Declining resumes nothing.
fn apply_resume_relocation(
    prefix: &mut Vec<euler_event::EventEnvelope>,
    live_root: &Path,
    accept_relocation_flag: bool,
    relocation: RelocationConsent,
    writer: &ProvenanceWriter,
) -> Result<()> {
    let Some(required) = euler_core::plan_relocation(prefix, live_root)? else {
        return Ok(());
    };
    let accept = if accept_relocation_flag {
        true
    } else {
        match relocation {
            RelocationConsent::Headless => false,
            RelocationConsent::Card(theme_choice) => {
                decide_relocation_card(&required, theme_choice)?
            }
            RelocationConsent::Line => decide_relocation_line(&required)?,
        }
    };
    if !accept {
        return Err(anyhow!(
            "Can't resume here: this session last ran in {}, but you're in {}. Re-run from that \
             folder, start a new session here, or pass --accept-relocation to move the session to \
             this folder.",
            required.recorded_root(),
            required.current_root()
        ));
    }
    let event = required.into_relocated_event();
    writer.append(std::slice::from_ref(&event))?;
    prefix.push(event);
    Ok(())
}

fn resume_cli_session<D>(
    target: ResumeTarget,
    run: RunArgs,
    decider: D,
    relocation: RelocationConsent,
    configure: impl FnOnce(&mut SessionConfig),
) -> Result<ResumeCliOutcome<D>>
where
    D: PermissionDecider,
{
    let ResumeTarget {
        log_path,
        refresh,
        display_label,
        session_name,
    } = target;
    bind_diagnostics_for_log(&log_path);
    let writer = ProvenanceWriter::new(log_path.clone())?;
    let mut prefix = read_resume_prefix(&log_path)?;
    let session_id = session_id_from_events(&prefix)
        .unwrap_or(SESSION_ID)
        .to_owned();
    let root = std::env::current_dir()?;
    let mut config = session_config(root, run.provider_id.clone(), run.model.clone(), session_id);
    apply_permission_reviewer(&mut config, &run);
    config.extensions_enabled = resolve_session_extensions(&config.root, &run.extensions)?;
    configure(&mut config);
    let observer = resolve_round_observer(&run.observe)?;
    if let Some((observer_config, _)) = &observer {
        config.round_observer = Some(observer_config.clone());
        if let Some(id) = run.observe.extension_id.as_ref() {
            config.extensions_enabled.insert(id.clone());
        }
    }
    apply_resume_relocation(
        &mut prefix,
        &config.root,
        run.accept_relocation,
        relocation,
        &writer,
    )?;
    let folded = fold_session(&config, prefix)?;
    let providers = (if let Some(original) = &folded.original_target {
        if invocation_target(&run) != *original {
            eprintln!(
                "warning: resume invocation target {}/{} differs from original session target {}/{}; using original target",
                run.provider_id, run.model, original.provider, original.model
            );
        }
        config.provider = original.provider.clone();
        config.model = original.model.clone();
        resume_provider_set_with_custom(
            original,
            &folded.active_target,
            run.auth_file.clone(),
            &run.custom_providers,
        )?
    } else {
        validate_resume_live_target(&run, &folded.active_target)?;
        validate_provider_auth(
            &folded.active_target.provider,
            run.auth_file.as_deref(),
            || run.provider.validate_auth(),
        )?;
        ProviderSet::single(run.provider)
    })
    .with_model_catalog(run.model_catalog.clone());
    // Limit tracks the active model after fold (may differ from launch if switched).
    config.provider = folded.active_target.provider.clone();
    config.model = folded.active_target.model.clone();
    apply_catalog_context_limit(&mut config, &run.model_catalog);

    let outcome = resume_session_from_folded_prefix(config, providers, decider, writer, folded)?;
    let warning_count = outcome.warnings.len();
    let mut session = outcome.session;
    crate::session_lifecycle::seed_secret_redaction(&mut session, run.auth_file.as_deref());
    if let Some((_, extension)) = observer {
        session.set_observer_extension(extension);
    }
    wire_code_swarm(&mut session);
    Ok(ResumeCliOutcome {
        session,
        refresh,
        events_folded: outcome.events_folded,
        active_target: outcome.active_target,
        recovery_closure_appended: outcome.recovery_closure_appended,
        warning_count,
        display_label,
        session_name,
    })
}

fn bind_diagnostics_for_log(log_path: &Path) {
    let session_dir = log_path.parent().unwrap_or_else(|| Path::new("."));
    diagnostics::bind_session_dir(session_dir);
}

pub(crate) fn validate_resume_live_target(run: &RunArgs, target: &ModelTarget) -> Result<()> {
    if target.provider != run.provider_id {
        return Err(anyhow!(
            "resume requires provider {} but this invocation configures {}",
            target.provider,
            run.provider_id
        ));
    }
    Ok(())
}

fn run_stdin_loop(
    session: &mut Session<CliDecider>,
    refresh: Option<&HomeSessionRefresh>,
) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let interactive_permissions = stdin.is_terminal();
    // The banner is output UX, so it is gated on stdout being a TTY (not
    // stdin) and sized from stdout's terminal. This avoids writing a banner
    // into a redirected file and avoids sizing it from a mismatched stdin.
    if stdout.is_terminal() {
        print!("{}", banner::render_string(terminal_width()));
        stdout.flush()?;
        eprintln!("note: each input line is sent as a separate turn");
    }

    let mut line = String::new();
    // Contract: each stdin line is one turn; callers must join multi-line
    // prompts before passing them to the CLI.
    loop {
        line.clear();
        let read = stdin.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        let input = line.trim_end();
        if input == "exit" {
            break;
        }
        if let Some(request) = strip_extension_run_prefix(input) {
            let output = execute_live_extension_run(session, request, interactive_permissions);
            if let Some(refresh) = refresh {
                if let Err(error) = refresh.refresh() {
                    eprintln!("warning: failed to refresh session metadata: {error}");
                }
            }
            writeln!(stdout, "{}", output)?;
            stdout.flush()?;
            continue;
        }
        if let Some(request) = strip_companion_run_prefix(input) {
            let output = execute_headless_companion_run(session, request);
            if let Some(refresh) = refresh {
                if let Err(error) = refresh.refresh() {
                    eprintln!("warning: failed to refresh session metadata: {error}");
                }
            }
            writeln!(stdout, "{}", output)?;
            stdout.flush()?;
            continue;
        }
        let turn = session.run_turn(input);
        if let Some(refresh) = refresh {
            if let Err(error) = refresh.refresh() {
                eprintln!("warning: failed to refresh session metadata: {error}");
            }
        }
        let events = turn?;
        print!("{}", render_line_oriented(&events));
        io::stdout().flush()?;
    }
    Ok(())
}

/// A control line is `extension_run` alone or `extension_run<ws>...`;
/// anything else (e.g. `extension_running ...`) stays ordinary user text.
fn strip_extension_run_prefix(input: &str) -> Option<&str> {
    strip_control_prefix(input, "extension_run")
}

fn strip_companion_run_prefix(input: &str) -> Option<&str> {
    strip_control_prefix(input, "companion_run")
}

fn strip_control_prefix<'a>(input: &'a str, token: &str) -> Option<&'a str> {
    let rest = input.strip_prefix(token)?;
    if rest.is_empty() {
        return Some("");
    }
    rest.starts_with(char::is_whitespace)
        .then(|| rest.trim_start())
}
