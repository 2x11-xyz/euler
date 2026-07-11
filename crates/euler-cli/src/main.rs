#![cfg_attr(test, allow(clippy::too_many_lines))] // unit-test exemption for inline test modules
use anyhow::{anyhow, Result};
use euler_core::permissions::{DeciderVerdict, PermissionRequest};
use euler_core::{
    fold_session, read_provenance, read_resume_prefix, resume_session_from_folded_prefix,
    CompactionTier, ModelTarget, PermissionDecider, ProvenanceWriter, Session, SessionConfig,
    SessionKind,
};
use euler_event::EventKind;
use euler_provider::anthropic::AnthropicProvider;
use euler_provider::catalog::{MergedModelCatalog, BUILTIN_PROVIDERS};
#[cfg(test)]
use euler_provider::catalog::{
    DEFAULT_ANTHROPIC_MODEL, DEFAULT_CHATGPT_MODEL, DEFAULT_FIXTURE_MODEL, DEFAULT_OPENAI_MODEL,
    DEFAULT_OPENROUTER_MODEL, DEFAULT_XAI_MODEL,
};
use euler_provider::chatgpt::ChatGptProvider;
use euler_provider::custom_provider::CustomOpenAiProvider;
use euler_provider::openai::OpenAiProvider;
use euler_provider::openrouter::OpenRouterProvider;
use euler_provider::provider_config::ProviderConfigRegistry;
use euler_provider::xai::XaiProvider;
use euler_provider::ReasoningEffort;
use euler_provider::{EchoProvider, ModelProvider, ProviderSet};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal, Read, Write};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
#[cfg(test)]
static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

mod auth_commands;
mod auth_validation;
mod bundled_extensions;
mod companion_run;
mod diagnostics;
mod extension_cli;
mod extension_enablement;
mod fixture_script;
mod help;
mod login;
mod model_catalog;
mod model_catalog_refresh;
pub mod model_preference;
mod model_resolution;
mod offline_extension_runner;
mod provider_config_runtime;
mod session_export;
mod session_lifecycle;
mod subagent;
mod theme_catalog;
mod ui;
use auth_commands::{logout_args_for_provider, logout_chatgpt, print_auth_status, LogoutArgs};
use auth_validation::{validate_provider_auth, StoredApiKeyAuth, StoredChatGptAuth};
use bundled_extensions::{
    bundled_descriptor_by_id, bundled_extension_by_id, bundled_round_observer,
    validate_observe_options, ObserveOptions,
};
use companion_run::execute_headless_companion_run;
use extension_cli::{run_extension_command, ExtensionArgs};
use extension_enablement::{resolve_session_extensions, ExtensionSelection};
use login::{login_args_for_provider, login_chatgpt, LoginArgs};
use model_preference::{ModelPreference, PreferenceLoad, ThemePreferenceLoad};
#[cfg(test)]
use model_resolution::LiveOptions;
use model_resolution::{
    canonical_model_preference, default_model_for_provider, parse_known_provider_id,
    parse_provider_id, raw_known_provider_id, resolve_live_options,
};
#[cfg(test)]
use session_export::execute_session_export;
use session_export::{
    build_session_export_args, run_session_export, ProvenanceExportArgs, RawProvenanceExportArgs,
};
pub(crate) use session_lifecycle::session_config;
use session_lifecycle::{
    apply_catalog_context_limit, live_session_config, resolve_resume_target, HomeSessionRefresh,
    LiveProvenance, ResumeTarget, SESSION_ID,
};
use subagent::{AutoApproveTier, SubagentDecider};
use theme_catalog::ThemeChoice;
use ui::app::{App, AppOptions};
use ui::banner;
use ui::transcript::render_line_oriented;
use ui::tui_decider::TuiDecider;

const EXPERIMENTAL_TUI_LINEFEED_HISTORY_FLAG: &str = "--experimental-tui-linefeed-history";
const NO_TUI_LINEFEED_HISTORY_FLAG: &str = "--no-tui-linefeed-history";

fn main() -> Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if let Some(text) = help::help_output(&raw)? {
        print!("{text}");
        return Ok(());
    }
    let args = Args::parse(raw.into_iter())?;
    let live_provenance = args.live_provenance();
    match args.command {
        Command::Replay { path } => {
            let events = read_provenance(&path)?;
            let unknown = events
                .iter()
                .filter_map(|event| {
                    let kind = event.kind.as_str();
                    (!EventKind::ALL.contains(&kind)).then(|| kind.to_owned())
                })
                .collect::<BTreeSet<_>>();
            for kind in unknown {
                eprintln!("warning: skipping unknown event kind {kind}");
            }
            print!("{}", render_line_oriented(&events));
            Ok(())
        }
        Command::Run(run) => {
            run_interactive_entry(live_provenance, run, args.default_interactive, args.no_tty)
        }
        Command::Tui(run) => run_tui(live_provenance, run),
        Command::Exec(exec) => run_exec(live_provenance, exec),
        Command::Resume { path, run } => resume_interactive(resolve_resume_target(path)?, run),
        Command::SessionExport(export) => run_session_export(export),
        Command::Login(login) => login_chatgpt(login),
        Command::Logout(logout) => logout_chatgpt(logout),
        Command::AuthStatus => print_auth_status(),
        Command::Models(ModelsCommand::List) => model_catalog::print_model_catalog(
            model_catalog::default_model_catalog_path().as_deref(),
            provider_config_runtime::default_provider_config_path().as_deref(),
            io::stdout(),
            io::stderr(),
        ),
        Command::Models(ModelsCommand::Refresh { force }) => {
            model_catalog_refresh::refresh_model_catalog(
                model_catalog::default_model_catalog_path().as_deref(),
                force,
                io::stdout(),
                io::stderr(),
            )
        }
        Command::Extension(extension) => run_extension_command(extension),
    }
}

fn run_interactive_entry(
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

fn apply_interactive_tui_linefeed_default(run: &mut RunArgs) {
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
    apply_catalog_context_limit(&mut live_session.config, &run.model_catalog);
    live_session.config.extensions_enabled =
        resolve_session_extensions(&live_session.config.root, &run.extensions)?;
    let observer = bundled_round_observer(&run.observe, &live_session.config.extensions_enabled)?;
    if let Some((observer_config, _)) = &observer {
        live_session.config.round_observer = Some(observer_config.clone());
    }
    bind_diagnostics_for_log(&live_session.log_path);
    let providers = ProviderSet::single_named(run.provider_id.clone(), run.provider);
    let mut session = Session::new_with_providers(live_session.config, providers, CliDecider)
        .with_provenance(ProvenanceWriter::new(live_session.log_path)?);
    crate::session_lifecycle::seed_secret_redaction(&mut session, run.auth_file.as_deref());
    if let Some((_, extension)) = observer {
        session.set_observer_extension(extension);
    }
    run_stdin_loop(&mut session, live_session.refresh.as_ref())
}

fn run_tui(provenance: LiveProvenance, run: RunArgs) -> Result<()> {
    let target = invocation_target(&run);
    validate_provider_auth(&target.provider, run.auth_file.as_deref(), || {
        run.provider.validate_auth()
    })?;
    let root = std::env::current_dir()?;
    let mut live_session =
        live_session_config(root, run.provider_id.clone(), run.model.clone(), provenance)?;
    live_session.config.session_kind = SessionKind::Interactive;
    apply_catalog_context_limit(&mut live_session.config, &run.model_catalog);
    live_session.config.extensions_enabled =
        resolve_session_extensions(&live_session.config.root, &run.extensions)?;
    let observer = bundled_round_observer(&run.observe, &live_session.config.extensions_enabled)?;
    if let Some((observer_config, _)) = &observer {
        live_session.config.round_observer = Some(observer_config.clone());
    }
    bind_diagnostics_for_log(&live_session.log_path);
    let (decider, channels) = TuiDecider::new();
    let providers = tui_provider_set(run.provider_id.clone(), run.provider, &run.custom_providers);
    let preference_path = model_preference::default_model_preference_path();
    let theme_choice = load_known_theme_preference(preference_path.as_deref()).unwrap_or_default();
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
        },
    )?;
    app.run()
}

fn run_exec(provenance: LiveProvenance, exec: ExecArgs) -> Result<()> {
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
    apply_catalog_context_limit(&mut live_session.config, &exec.run.model_catalog);
    apply_exec_config(
        &mut live_session.config,
        ExecConfigOverrides::from_run(&exec.run),
    );
    live_session.config.extensions_enabled =
        resolve_session_extensions(&live_session.config.root, &exec.run.extensions)?;
    let observer =
        bundled_round_observer(&exec.run.observe, &live_session.config.extensions_enabled)?;
    if let Some((observer_config, _)) = &observer {
        live_session.config.round_observer = Some(observer_config.clone());
    }
    let tier = exec.auto_approve;
    let providers = ProviderSet::single_named(exec.run.provider_id.clone(), exec.run.provider);
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
    SubagentDecider::apply_tier(tier, &mut session);
    let events = session.run_turn(&prompt)?;
    if let Some(refresh) = refresh.as_ref() {
        if let Err(error) = refresh.refresh() {
            eprintln!("warning: failed to refresh session metadata: {error}");
        }
    }
    print!("{}", render_line_oriented(&events));
    io::stdout().flush()?;
    Ok(())
}

fn run_exec_resume(
    target: ResumeTarget,
    run: RunArgs,
    auto_approve: AutoApproveTier,
    prompt: String,
) -> Result<()> {
    let overrides = ExecConfigOverrides::from_run(&run);
    let mut outcome =
        resume_cli_session(target, run, SubagentDecider::new(auto_approve), |config| {
            apply_exec_config(config, overrides);
        })?;
    SubagentDecider::apply_tier(auto_approve, &mut outcome.session);
    let events = outcome.session.run_turn(&prompt)?;
    if let Some(refresh) = outcome.refresh.as_ref() {
        if let Err(error) = refresh.refresh() {
            eprintln!("warning: failed to refresh session metadata: {error}");
        }
    }
    print!("{}", render_line_oriented(&events));
    io::stdout().flush()?;
    Ok(())
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

fn apply_exec_config(config: &mut SessionConfig, overrides: ExecConfigOverrides) {
    config.max_output_tokens = overrides.max_output_tokens;
    if overrides.max_tool_rounds.is_some() {
        config.max_tool_rounds = overrides.max_tool_rounds;
    }
    if let Some(tier) = overrides.auto_compaction {
        config.auto_compaction.tier = tier;
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

fn non_empty_exec_prompt(prompt: String) -> Result<String> {
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

fn resume_interactive(target: ResumeTarget, run: RunArgs) -> Result<()> {
    let mut outcome = resume_cli_session(target, run, CliDecider, |_| {})?;
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

struct ResumeCliOutcome<D: PermissionDecider> {
    session: Session<D>,
    refresh: Option<HomeSessionRefresh>,
    events_folded: usize,
    active_target: ModelTarget,
    recovery_closure_appended: bool,
}

fn resume_cli_session<D>(
    target: ResumeTarget,
    run: RunArgs,
    decider: D,
    configure: impl FnOnce(&mut SessionConfig),
) -> Result<ResumeCliOutcome<D>>
where
    D: PermissionDecider,
{
    let ResumeTarget { log_path, refresh } = target;
    bind_diagnostics_for_log(&log_path);
    let writer = ProvenanceWriter::new(log_path.clone())?;
    let prefix = read_resume_prefix(&log_path)?;
    let session_id = session_id_from_events(&prefix)
        .unwrap_or(SESSION_ID)
        .to_owned();
    let root = std::env::current_dir()?;
    let mut config = session_config(root, run.provider_id.clone(), run.model.clone(), session_id);
    config.extensions_enabled = resolve_session_extensions(&config.root, &run.extensions)?;
    configure(&mut config);
    let observer = bundled_round_observer(&run.observe, &config.extensions_enabled)?;
    if let Some((observer_config, _)) = &observer {
        config.round_observer = Some(observer_config.clone());
    }
    let folded = fold_session(&config, prefix)?;
    let providers = if let Some(original) = &folded.original_target {
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
    };
    // Limit tracks the active model after fold (may differ from launch if switched).
    config.provider = folded.active_target.provider.clone();
    config.model = folded.active_target.model.clone();
    apply_catalog_context_limit(&mut config, &run.model_catalog);

    let outcome = resume_session_from_folded_prefix(config, providers, decider, writer, folded)?;
    let mut session = outcome.session;
    crate::session_lifecycle::seed_secret_redaction(&mut session, run.auth_file.as_deref());
    if let Some((_, extension)) = observer {
        session.set_observer_extension(extension);
    }
    Ok(ResumeCliOutcome {
        session,
        refresh,
        events_folded: outcome.events_folded,
        active_target: outcome.active_target,
        recovery_closure_appended: outcome.recovery_closure_appended,
    })
}

fn bind_diagnostics_for_log(log_path: &Path) {
    let session_dir = log_path.parent().unwrap_or_else(|| Path::new("."));
    diagnostics::bind_session_dir(session_dir);
}

fn validate_resume_live_target(run: &RunArgs, target: &ModelTarget) -> Result<()> {
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
            let output = execute_headless_extension_run(session, request);
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

fn execute_headless_extension_run(
    session: &mut Session<CliDecider>,
    request: &str,
) -> serde_json::Value {
    match parse_live_extension_request(request) {
        Ok((id, command, input)) => run_live_extension_command(session, &id, &command, input),
        Err(error) => headless_extension_error(error.to_string()),
    }
}

fn parse_live_extension_request(request: &str) -> Result<(String, String, serde_json::Value)> {
    let mut parts = request.splitn(2, char::is_whitespace);
    let reference = parts
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("extension_run requires an extension command reference"))?;
    let input = parts
        .next()
        .ok_or_else(|| anyhow!("extension_run {reference} requires JSON input"))?
        .trim_start();
    if input.is_empty() {
        return Err(anyhow!("extension_run {reference} requires JSON input"));
    }
    let (id, command) = parse_live_extension_reference(reference)?;
    let input = serde_json::from_str(input)
        .map_err(|error| anyhow!("extension_run {reference} input must be JSON: {error}"))?;
    Ok((id, command, input))
}

fn parse_live_extension_reference(reference: &str) -> Result<(String, String)> {
    let Some((id, command)) = reference.split_once('.') else {
        return Err(anyhow!("invalid extension command reference: {reference}"));
    };
    if id.is_empty() || command.is_empty() || command.contains('.') {
        return Err(anyhow!("invalid extension command reference: {reference}"));
    }
    Ok((id.to_owned(), command.to_owned()))
}

fn run_live_extension_command(
    session: &mut Session<CliDecider>,
    id: &str,
    command: &str,
    input: serde_json::Value,
) -> serde_json::Value {
    let descriptor = match bundled_descriptor_by_id(id) {
        Ok(Some(descriptor)) => descriptor,
        Ok(None) => return headless_extension_error(format!("unknown extension id: {id}")),
        Err(error) => return headless_extension_error(error.to_string()),
    };
    let Some(command_descriptor) = descriptor.command(command) else {
        return headless_extension_error(format!("unknown command for extension {id}: {command}"));
    };
    let Some(bundled) = bundled_extension_by_id(id) else {
        return headless_extension_error(format!("unknown extension id: {id}"));
    };
    // Piped headless runs cannot prompt (stdin is the command protocol):
    // invoking `extension_run` names the command explicitly, so its declared
    // capabilities are granted for this run — with visibility, never silently.
    if !command_descriptor.required_capabilities.is_empty() {
        let granted = command_descriptor
            .required_capabilities
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "extension {id}.{command}: granting declared capabilities for this run: {granted}"
        );
    }
    match session.execute_extension_command(
        bundled.extension,
        command,
        input,
        command_descriptor.required_capabilities.iter().copied(),
    ) {
        Ok(result) => serde_json::json!({
            "type": "extension_run_result",
            "extension": id,
            "command": command,
            "result": result,
        }),
        Err(error) => headless_extension_error(error.to_string()),
    }
}

fn headless_extension_error(message: String) -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "source": "extension_run",
        "message": message,
    })
}

/// Best-effort terminal column count for banner centering. Uses libc
/// TIOCGWINSZ on Unix and falls back to 80 when the width cannot be
/// determined (non-TTY, narrow pipe, non-Unix, ioctl failure). Bounded to a
/// 20-column floor so centering math stays sane on degenerate inputs.
fn terminal_width() -> usize {
    std::cmp::max(terminal_columns().unwrap_or(80), 20)
}

#[cfg(unix)]
fn terminal_columns() -> Option<usize> {
    use std::os::fd::AsRawFd;
    // Safety: TIOCGWINSZ writes a winsize struct through the ioctl pointer;
    // the struct is stack-local and the fd is stdout. This specific request
    // is observational with respect to terminal state.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        let fd = std::io::stdout().as_raw_fd();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws as *mut _) == 0 {
            let cols = ws.ws_col;
            if cols != 0 {
                return Some(cols as usize);
            }
        }
    }
    None
}

#[cfg(not(unix))]
fn terminal_columns() -> Option<usize> {
    None
}

struct Args {
    provenance_path: PathBuf,
    provenance_from_cli: bool,
    command: Command,
    default_interactive: bool,
    no_tty: bool,
}

impl Args {
    fn live_provenance(&self) -> LiveProvenance {
        if self.provenance_from_cli {
            LiveProvenance::Explicit(self.provenance_path.clone())
        } else {
            LiveProvenance::HomeSession
        }
    }
}

/// Replay reads an existing log and ignores live-provider arguments entirely,
/// including their validation.
enum Command {
    Replay { path: PathBuf },
    Run(RunArgs),
    Tui(RunArgs),
    Exec(ExecArgs),
    Resume { path: PathBuf, run: RunArgs },
    Login(LoginArgs),
    Logout(LogoutArgs),
    AuthStatus,
    Models(ModelsCommand),
    SessionExport(ProvenanceExportArgs),
    Extension(ExtensionArgs),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModelsCommand {
    List,
    Refresh { force: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TopLevelCommand {
    Run,
    Tui,
    Exec,
    Login,
    Logout,
    AuthStatus,
    Models,
    SessionExport,
    Extension,
}

impl TopLevelCommand {
    fn label(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Tui => "tui",
            Self::Exec => "exec",
            Self::Login => "login",
            Self::Logout => "logout",
            Self::AuthStatus => "auth status",
            Self::Models => "models",
            Self::SessionExport => "session-export",
            Self::Extension => "extension",
        }
    }
}

fn accept_top_level_command(
    seen: &mut Option<TopLevelCommand>,
    command: TopLevelCommand,
) -> Result<()> {
    match *seen {
        Some(existing) if existing == command => Err(anyhow!(
            "{} command was provided more than once",
            command.label()
        )),
        Some(existing) => Err(anyhow!(
            "{} cannot be combined with {}",
            existing.label(),
            command.label()
        )),
        None => {
            *seen = Some(command);
            Ok(())
        }
    }
}

fn linefeed_history_flag_label(enabled: bool) -> &'static str {
    if enabled {
        EXPERIMENTAL_TUI_LINEFEED_HISTORY_FLAG
    } else {
        NO_TUI_LINEFEED_HISTORY_FLAG
    }
}

fn accept_linefeed_history_option(
    current: &mut Option<bool>,
    enabled: bool,
    flag: &str,
) -> Result<()> {
    match *current {
        Some(existing) if existing == enabled => Err(anyhow!("{flag} was provided more than once")),
        Some(existing) => Err(anyhow!(
            "{} cannot be combined with {flag}",
            linefeed_history_flag_label(existing)
        )),
        None => {
            *current = Some(enabled);
            Ok(())
        }
    }
}

fn parse_positive_u64(value: &str, flag: &str) -> Result<u64> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| anyhow!("{flag} requires a positive integer"))?;
    if parsed == 0 {
        Err(anyhow!("{flag} requires a positive integer"))
    } else {
        Ok(parsed)
    }
}

struct RunArgs {
    provider_id: String,
    provider: Box<dyn ModelProvider>,
    model: String,
    model_catalog: MergedModelCatalog,
    auth_file: Option<PathBuf>,
    custom_providers: ProviderConfigRegistry,
    max_output_tokens: Option<u64>,
    max_tool_rounds: Option<usize>,
    auto_compaction: Option<CompactionTier>,
    compaction_budget_bytes: Option<usize>,
    reasoning_effort: Option<ReasoningEffort>,
    extensions: ExtensionSelection,
    observe: ObserveOptions,
    linefeed_history_insert: bool,
    linefeed_history_insert_from_cli: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProviderOptions {
    values: BTreeMap<String, String>,
}

impl ProviderOptions {
    fn insert(&mut self, option: &str) -> Result<()> {
        let Some((key, value)) = option.split_once('=') else {
            return Err(anyhow!("--provider-option requires key=value"));
        };
        if key.is_empty() {
            return Err(anyhow!("--provider-option key cannot be empty"));
        }
        if key.trim() != key || value.trim() != value {
            return Err(anyhow!(
                "--provider-option does not allow whitespace around key or value"
            ));
        }
        if !key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        {
            return Err(anyhow!("invalid provider option key: {key}"));
        }
        if self
            .values
            .insert(key.to_owned(), value.to_owned())
            .is_some()
        {
            return Err(anyhow!("duplicate provider option: {key}"));
        }
        Ok(())
    }

    fn keys(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }
}

struct ExecArgs {
    run: RunArgs,
    auto_approve: AutoApproveTier,
    prompt: Option<String>,
    resume_path: Option<PathBuf>,
}

fn ensure_no_provider_options(parsed: &RawArgs, context: &str) -> Result<()> {
    if parsed.provider_options.values.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("--provider-option is not supported with {context}"))
    }
}

fn ensure_no_extensions(parsed: &RawArgs, context: &str) -> Result<()> {
    if parsed.extensions.is_cli_set() {
        Err(anyhow!("--extensions is not supported with {context}"))
    } else {
        Ok(())
    }
}

impl Args {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let env = EnvArgs {
            provider: std::env::var("EULER_PROVIDER").ok(),
            model: std::env::var("EULER_MODEL").ok(),
            auth_file: std::env::var_os("EULER_AUTH_FILE").map(PathBuf::from),
        };
        let preference_path = model_preference::default_model_preference_path();
        let model_catalog_path = model_catalog::default_model_catalog_path();
        let provider_config_path = provider_config_runtime::default_provider_config_path();
        Self::parse_with_env_preference_catalog_and_provider_config(
            &mut args,
            env,
            preference_path.as_deref(),
            model_catalog_path.as_deref(),
            provider_config_path.as_deref(),
        )
    }

    #[cfg(test)]
    fn parse_with_env(args: &mut impl Iterator<Item = String>, env: EnvArgs) -> Result<Self> {
        Self::parse_with_env_and_preference(args, env, None)
    }

    #[cfg(test)]
    fn parse_with_env_and_preference_path(
        args: &mut impl Iterator<Item = String>,
        env: EnvArgs,
        preference_path: &Path,
    ) -> Result<Self> {
        Self::parse_with_env_and_preference(args, env, Some(preference_path))
    }

    #[cfg(test)]
    fn parse_with_env_and_preference(
        args: &mut impl Iterator<Item = String>,
        env: EnvArgs,
        preference_path: Option<&Path>,
    ) -> Result<Self> {
        Self::parse_with_env_preference_and_catalog(args, env, preference_path, None)
    }

    #[cfg(test)]
    fn parse_with_env_preference_and_catalog_path(
        args: &mut impl Iterator<Item = String>,
        env: EnvArgs,
        preference_path: Option<&Path>,
        model_catalog_path: &Path,
    ) -> Result<Self> {
        Self::parse_with_env_preference_and_catalog(
            args,
            env,
            preference_path,
            Some(model_catalog_path),
        )
    }

    #[cfg(test)]
    fn parse_with_env_preference_and_catalog(
        args: &mut impl Iterator<Item = String>,
        env: EnvArgs,
        preference_path: Option<&Path>,
        model_catalog_path: Option<&Path>,
    ) -> Result<Self> {
        Self::parse_with_env_preference_catalog_and_provider_config(
            args,
            env,
            preference_path,
            model_catalog_path,
            None,
        )
    }

    fn parse_with_env_preference_catalog_and_provider_config(
        args: &mut impl Iterator<Item = String>,
        env: EnvArgs,
        preference_path: Option<&Path>,
        model_catalog_path: Option<&Path>,
        provider_config_path: Option<&Path>,
    ) -> Result<Self> {
        let parsed = RawArgs::parse_with_env(args, env)?;
        if let Some(linefeed_history_insert) = parsed.linefeed_history_insert {
            let flag = linefeed_history_flag_label(linefeed_history_insert);
            if !parsed.allows_tui_linefeed_history_option() {
                return Err(anyhow!("{flag} is only supported with tui"));
            }
        }
        let mut default_interactive = false;
        validate_replay_resume_conflicts(&parsed)?;
        let command = if parsed.login {
            Command::Login(build_login_args(&parsed)?)
        } else if parsed.logout {
            Command::Logout(build_logout_args(&parsed)?)
        } else if parsed.auth_status {
            build_auth_status_args(&parsed)?;
            Command::AuthStatus
        } else if parsed.models {
            build_models_args(&parsed)?;
            Command::Models(parsed.models_command)
        } else if parsed.exec {
            let preference = load_known_model_preference(preference_path);
            let model_catalog = load_known_model_catalog(model_catalog_path);
            let custom_providers = load_custom_provider_config(provider_config_path);
            Command::Exec(build_exec_args(
                &parsed,
                preference.as_ref(),
                &model_catalog,
                &custom_providers,
            )?)
        } else if parsed.session_export.is_active() {
            ensure_no_provider_options(&parsed, "session-export")?;
            Command::SessionExport(build_session_export_args(&parsed)?)
        } else if let Some(extension) = parsed.extension.as_ref() {
            ensure_no_provider_options(&parsed, "extension")?;
            validate_extension_args(&parsed)?;
            Command::Extension(extension.clone())
        } else if parsed.tui {
            if parsed.no_tty {
                return Err(anyhow!("tui cannot be combined with --no-tty"));
            }
            let preference = load_known_model_preference(preference_path);
            let model_catalog = load_known_model_catalog(model_catalog_path);
            let custom_providers = load_custom_provider_config(provider_config_path);
            Command::Tui(build_run_args(
                &parsed,
                preference.as_ref(),
                &model_catalog,
                &custom_providers,
            )?)
        } else if let Some(path) = parsed.replay_path.clone() {
            ensure_no_provider_options(&parsed, "--replay")?;
            ensure_no_extensions(&parsed, "--replay")?;
            Command::Replay { path }
        } else if let Some(path) = parsed.resume_path.clone() {
            ensure_no_provider_options(&parsed, "--resume")?;
            let model_catalog = load_known_model_catalog(model_catalog_path);
            let custom_providers = load_custom_provider_config(provider_config_path);
            Command::Resume {
                path,
                run: build_run_args(&parsed, None, &model_catalog, &custom_providers)?,
            }
        } else {
            let preference = load_known_model_preference(preference_path);
            let model_catalog = load_known_model_catalog(model_catalog_path);
            let custom_providers = load_custom_provider_config(provider_config_path);
            default_interactive = !parsed.explicit_run;
            Command::Run(build_run_args(
                &parsed,
                preference.as_ref(),
                &model_catalog,
                &custom_providers,
            )?)
        };

        Ok(Self {
            provenance_path: parsed.provenance_path,
            provenance_from_cli: parsed.provenance_from_cli,
            command,
            default_interactive,
            no_tty: parsed.no_tty,
        })
    }
}

fn validate_replay_resume_conflicts(parsed: &RawArgs) -> Result<()> {
    let replay_or_resume = parsed.replay_path.is_some() || parsed.resume_path.is_some();
    if parsed.exec && parsed.replay_path.is_some() {
        return Err(anyhow!("exec cannot be used with --replay"));
    }
    if parsed.login && replay_or_resume {
        return Err(anyhow!("login cannot be used with --replay or --resume"));
    }
    if parsed.logout && replay_or_resume {
        return Err(anyhow!("logout cannot be used with --replay or --resume"));
    }
    if parsed.auth_status && replay_or_resume {
        return Err(anyhow!(
            "auth status cannot be used with --replay or --resume"
        ));
    }
    if parsed.models && replay_or_resume {
        return Err(anyhow!("models cannot be used with --replay or --resume"));
    }
    if parsed.session_export.is_active() && replay_or_resume {
        return Err(anyhow!(
            "session-export cannot be used with --replay or --resume"
        ));
    }
    if parsed.extension.is_some() && replay_or_resume {
        return Err(anyhow!(
            "extension cannot be used with --replay or --resume"
        ));
    }
    if parsed.replay_path.is_some() && parsed.resume_path.is_some() {
        return Err(anyhow!("--replay and --resume cannot be used together"));
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct RawArgs {
    provenance_path: PathBuf,
    provenance_from_cli: bool,
    provider: Option<String>,
    provider_from_cli: bool,
    model: Option<String>,
    model_from_cli: bool,
    provider_options: ProviderOptions,
    auth_file: Option<PathBuf>,
    auth_file_from_cli: bool,
    replay_path: Option<PathBuf>,
    resume_path: Option<PathBuf>,
    explicit_run: bool,
    tui: bool,
    exec: bool,
    exec_prompt: Vec<String>,
    auto_approve: Option<AutoApproveTier>,
    max_output_tokens: Option<u64>,
    max_tool_rounds: Option<usize>,
    auto_compaction: Option<CompactionTier>,
    compaction_budget_bytes: Option<usize>,
    reasoning_effort: Option<ReasoningEffort>,
    extensions: ExtensionSelection,
    observe: ObserveOptions,
    login: bool,
    logout: bool,
    auth_status: bool,
    models: bool,
    models_command: ModelsCommand,
    session_export: RawProvenanceExportArgs,
    extension: Option<ExtensionArgs>,
    no_tty: bool,
    linefeed_history_insert: Option<bool>,
}

impl RawArgs {
    fn parse_with_env(args: &mut impl Iterator<Item = String>, env: EnvArgs) -> Result<Self> {
        RawArgsParser::new(env).parse(args)
    }

    fn allows_tui_linefeed_history_option(&self) -> bool {
        self.tui || self.can_default_to_tui()
    }

    fn can_default_to_tui(&self) -> bool {
        !self.explicit_run
            && !self.exec
            && !self.login
            && !self.logout
            && !self.auth_status
            && !self.models
            && !self.session_export.is_active()
            && self.extension.is_none()
            && self.replay_path.is_none()
            && self.resume_path.is_none()
            && !self.no_tty
    }
}

struct RawArgsParser {
    parsed: RawArgs,
    top_level_command: Option<TopLevelCommand>,
    saw_any_arg: bool,
}

impl RawArgsParser {
    fn new(env: EnvArgs) -> Self {
        Self {
            parsed: RawArgs {
                provenance_path: PathBuf::from("euler-provenance.jsonl"),
                provenance_from_cli: false,
                provider: env.provider,
                provider_from_cli: false,
                model: env.model,
                model_from_cli: false,
                provider_options: ProviderOptions::default(),
                auth_file: env.auth_file,
                auth_file_from_cli: false,
                replay_path: None,
                resume_path: None,
                explicit_run: false,
                tui: false,
                exec: false,
                exec_prompt: Vec::new(),
                auto_approve: None,
                max_output_tokens: None,
                max_tool_rounds: None,
                auto_compaction: None,
                compaction_budget_bytes: None,
                reasoning_effort: None,
                extensions: ExtensionSelection::default(),
                observe: ObserveOptions::default(),
                login: false,
                logout: false,
                auth_status: false,
                models: false,
                models_command: ModelsCommand::List,
                session_export: RawProvenanceExportArgs::default(),
                extension: None,
                no_tty: false,
                linefeed_history_insert: None,
            },
            top_level_command: None,
            saw_any_arg: false,
        }
    }

    fn parse(mut self, args: &mut impl Iterator<Item = String>) -> Result<RawArgs> {
        while let Some(arg) = args.next() {
            if self.parse_arg(args, arg)? == ArgParseFlow::Stop {
                break;
            }
            self.saw_any_arg = true;
        }
        Ok(self.parsed)
    }

    fn parse_arg(
        &mut self,
        args: &mut impl Iterator<Item = String>,
        arg: String,
    ) -> Result<ArgParseFlow> {
        match arg.as_str() {
            "run" if !self.parsed.exec => self.parse_run_command(),
            "tui" if !self.parsed.exec => self.parse_tui_command(),
            "exec" => self.parse_exec_command(),
            "login" => self.parse_first_command(TopLevelCommand::Login, "login", |parsed| {
                parsed.login = true;
            }),
            "logout" => self.parse_first_command(TopLevelCommand::Logout, "logout", |parsed| {
                parsed.logout = true;
            }),
            "auth" => self.parse_auth_command(args),
            "models" if !self.parsed.exec => self.parse_models_command(args),
            "session-export" if !self.parsed.exec => self.parse_session_export_command(args),
            "extension" if !self.parsed.exec => self.parse_extension_command(args),
            "--provenance" => self.parse_provenance(args),
            "--provider" => self.parse_provider(args),
            "--model" => self.parse_model(args),
            "--provider-option" => self.parse_provider_option(args),
            "--replay" => self.parse_replay(args),
            "--resume" => self.parse_resume(args),
            "--auth-file" => self.parse_auth_file(args),
            "--no-tty" => {
                self.parsed.no_tty = true;
                Ok(ArgParseFlow::Continue)
            }
            EXPERIMENTAL_TUI_LINEFEED_HISTORY_FLAG => self.parse_linefeed_history(true, &arg),
            NO_TUI_LINEFEED_HISTORY_FLAG => self.parse_linefeed_history(false, &arg),
            "--limit" => self.parse_session_export_option(args, SessionExportOption::Limit),
            "--scan-limit" => {
                self.parse_session_export_option(args, SessionExportOption::ScanLimit)
            }
            "--after-event-id" => {
                self.parse_session_export_option(args, SessionExportOption::AfterEventId)
            }
            "--kind" => self.parse_session_export_option(args, SessionExportOption::Kind),
            "--auto-approve" => self.parse_auto_approve(args),
            "--max-output-tokens" => self.parse_max_output_tokens(args),
            "--max-tool-rounds" => self.parse_max_tool_rounds(args),
            "--auto-compaction" => self.parse_auto_compaction(args),
            "--compaction-budget-bytes" => self.parse_compaction_budget_bytes(args),
            "--reasoning-effort" => self.parse_reasoning_effort(args),
            "--extensions" => self.parse_extensions(args),
            "--observe" => self.parse_observe(args),
            "--observe-cadence" => self.parse_observe_cadence(args),
            "--" if self.parsed.exec => {
                self.parsed.exec_prompt.extend(args.by_ref());
                Ok(ArgParseFlow::Stop)
            }
            _ if self.parsed.exec && arg.starts_with('-') => {
                Err(anyhow!("unknown argument: {arg} (try 'euler --help')"))
            }
            _ if self.parsed.exec => {
                self.parsed.exec_prompt.push(arg);
                Ok(ArgParseFlow::Continue)
            }
            _ => Err(anyhow!("unknown argument: {arg} (try 'euler --help')")),
        }
    }

    fn parse_run_command(&mut self) -> Result<ArgParseFlow> {
        accept_top_level_command(&mut self.top_level_command, TopLevelCommand::Run)?;
        self.parsed.explicit_run = true;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_tui_command(&mut self) -> Result<ArgParseFlow> {
        accept_top_level_command(&mut self.top_level_command, TopLevelCommand::Tui)?;
        self.parsed.tui = true;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_exec_command(&mut self) -> Result<ArgParseFlow> {
        accept_top_level_command(&mut self.top_level_command, TopLevelCommand::Exec)?;
        self.parsed.exec = true;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_first_command(
        &mut self,
        command: TopLevelCommand,
        name: &str,
        set_flag: impl FnOnce(&mut RawArgs),
    ) -> Result<ArgParseFlow> {
        accept_top_level_command(&mut self.top_level_command, command)?;
        if self.saw_any_arg {
            return Err(anyhow!("{name} must be the first argument"));
        }
        set_flag(&mut self.parsed);
        Ok(ArgParseFlow::Continue)
    }

    fn parse_auth_command(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        let Some(subcommand) = args.next() else {
            return Err(anyhow!("auth requires a subcommand"));
        };
        if subcommand != "status" {
            return Err(anyhow!("unknown auth subcommand: {subcommand}"));
        }
        let had_top_level_command = self.top_level_command.is_some();
        accept_top_level_command(&mut self.top_level_command, TopLevelCommand::AuthStatus)?;
        if self.saw_any_arg && !had_top_level_command {
            return Err(anyhow!("auth must be the first argument"));
        }
        self.parsed.auth_status = true;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_models_command(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        accept_top_level_command(&mut self.top_level_command, TopLevelCommand::Models)?;
        if self.saw_any_arg {
            return Err(anyhow!("models must be the first argument"));
        }
        self.parsed.models = true;
        self.parsed.models_command = parse_models_command(args)?;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_session_export_command(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        accept_top_level_command(&mut self.top_level_command, TopLevelCommand::SessionExport)?;
        self.parsed.session_export.start(args)?;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_extension_command(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        accept_top_level_command(&mut self.top_level_command, TopLevelCommand::Extension)?;
        self.parsed.extension = Some(ExtensionArgs::parse(args)?);
        Ok(ArgParseFlow::Stop)
    }

    fn parse_provenance(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        let Some(value) = args.next() else {
            return Err(anyhow!("--provenance requires a path"));
        };
        self.parsed.provenance_path = PathBuf::from(value);
        self.parsed.provenance_from_cli = true;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_provider(&mut self, args: &mut impl Iterator<Item = String>) -> Result<ArgParseFlow> {
        self.parsed.provider = Some(
            args.next()
                .ok_or_else(|| anyhow!("--provider requires a value"))?,
        );
        self.parsed.provider_from_cli = true;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_model(&mut self, args: &mut impl Iterator<Item = String>) -> Result<ArgParseFlow> {
        self.parsed.model = Some(
            args.next()
                .ok_or_else(|| anyhow!("--model requires a value"))?,
        );
        self.parsed.model_from_cli = true;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_provider_option(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        let value = args
            .next()
            .ok_or_else(|| anyhow!("--provider-option requires key=value"))?;
        self.parsed.provider_options.insert(&value)?;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_replay(&mut self, args: &mut impl Iterator<Item = String>) -> Result<ArgParseFlow> {
        self.parsed.replay_path = Some(PathBuf::from(
            args.next()
                .ok_or_else(|| anyhow!("--replay requires a path"))?,
        ));
        Ok(ArgParseFlow::Continue)
    }

    fn parse_resume(&mut self, args: &mut impl Iterator<Item = String>) -> Result<ArgParseFlow> {
        self.parsed.resume_path = Some(PathBuf::from(
            args.next()
                .ok_or_else(|| anyhow!("--resume requires a path"))?,
        ));
        Ok(ArgParseFlow::Continue)
    }

    fn parse_auth_file(&mut self, args: &mut impl Iterator<Item = String>) -> Result<ArgParseFlow> {
        self.parsed.auth_file_from_cli = true;
        self.parsed.auth_file = Some(PathBuf::from(
            args.next()
                .ok_or_else(|| anyhow!("--auth-file requires a path"))?,
        ));
        Ok(ArgParseFlow::Continue)
    }

    fn parse_observe(&mut self, args: &mut impl Iterator<Item = String>) -> Result<ArgParseFlow> {
        if self.parsed.observe.extension_id.is_some() {
            return Err(anyhow!("--observe was provided more than once"));
        }
        self.parsed.observe.extension_id = Some(
            args.next()
                .ok_or_else(|| anyhow!("--observe requires an extension id"))?,
        );
        Ok(ArgParseFlow::Continue)
    }

    fn parse_observe_cadence(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        if self.parsed.observe.cadence_rounds.is_some() {
            return Err(anyhow!("--observe-cadence was provided more than once"));
        }
        let value = args
            .next()
            .ok_or_else(|| anyhow!("--observe-cadence requires a value"))?;
        self.parsed.observe.cadence_rounds = Some(
            NonZeroU64::new(parse_positive_u64(&value, "--observe-cadence")?)
                .expect("positive cadence is non-zero"),
        );
        Ok(ArgParseFlow::Continue)
    }

    fn parse_linefeed_history(&mut self, enabled: bool, arg: &str) -> Result<ArgParseFlow> {
        accept_linefeed_history_option(&mut self.parsed.linefeed_history_insert, enabled, arg)?;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_session_export_option(
        &mut self,
        args: &mut impl Iterator<Item = String>,
        option: SessionExportOption,
    ) -> Result<ArgParseFlow> {
        match option {
            SessionExportOption::Limit => self.parsed.session_export.set_limit(args)?,
            SessionExportOption::ScanLimit => self.parsed.session_export.set_scan_limit(args)?,
            SessionExportOption::AfterEventId => {
                self.parsed.session_export.set_after_event_id(args)?
            }
            SessionExportOption::Kind => self.parsed.session_export.add_kind(args)?,
        }
        Ok(ArgParseFlow::Continue)
    }

    fn parse_auto_approve(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        if !self.parsed.exec {
            return Err(anyhow!("--auto-approve is only supported with exec"));
        }
        if self.parsed.auto_approve.is_some() {
            return Err(anyhow!("--auto-approve was provided more than once"));
        }
        let value = args
            .next()
            .ok_or_else(|| anyhow!("--auto-approve requires a tier"))?;
        self.parsed.auto_approve = Some(AutoApproveTier::parse(&value).ok_or_else(|| {
            anyhow!(
                "unknown auto-approve tier: {value}; supported tiers: {}",
                AutoApproveTier::SUPPORTED
            )
        })?);
        Ok(ArgParseFlow::Continue)
    }

    fn parse_max_output_tokens(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        if !self.parsed.exec {
            return Err(anyhow!("--max-output-tokens is only supported with exec"));
        }
        if self.parsed.max_output_tokens.is_some() {
            return Err(anyhow!("--max-output-tokens was provided more than once"));
        }
        let value = args
            .next()
            .ok_or_else(|| anyhow!("--max-output-tokens requires a value"))?;
        self.parsed.max_output_tokens = Some(parse_positive_u64(&value, "--max-output-tokens")?);
        Ok(ArgParseFlow::Continue)
    }

    fn parse_max_tool_rounds(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        if !self.parsed.exec {
            return Err(anyhow!("--max-tool-rounds is only supported with exec"));
        }
        if self.parsed.max_tool_rounds.is_some() {
            return Err(anyhow!("--max-tool-rounds was provided more than once"));
        }
        let value = args
            .next()
            .ok_or_else(|| anyhow!("--max-tool-rounds requires a value"))?;
        self.parsed.max_tool_rounds =
            Some(parse_positive_u64(&value, "--max-tool-rounds")? as usize);
        Ok(ArgParseFlow::Continue)
    }

    fn parse_auto_compaction(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        if !self.parsed.exec {
            return Err(anyhow!("--auto-compaction is only supported with exec"));
        }
        if self.parsed.auto_compaction.is_some() {
            return Err(anyhow!("--auto-compaction was provided more than once"));
        }
        let value = args
            .next()
            .ok_or_else(|| anyhow!("--auto-compaction requires a value"))?;
        let tier = CompactionTier::parse(&value)
            .ok_or_else(|| anyhow!("--auto-compaction must be one of off|stubs"))?;
        self.parsed.auto_compaction = Some(tier);
        Ok(ArgParseFlow::Continue)
    }

    fn parse_compaction_budget_bytes(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        if !self.parsed.exec {
            return Err(anyhow!(
                "--compaction-budget-bytes is only supported with exec"
            ));
        }
        if self.parsed.compaction_budget_bytes.is_some() {
            return Err(anyhow!(
                "--compaction-budget-bytes was provided more than once"
            ));
        }
        let value = args
            .next()
            .ok_or_else(|| anyhow!("--compaction-budget-bytes requires a value"))?;
        self.parsed.compaction_budget_bytes =
            Some(parse_positive_u64(&value, "--compaction-budget-bytes")? as usize);
        Ok(ArgParseFlow::Continue)
    }

    fn parse_reasoning_effort(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        if !self.parsed.exec {
            return Err(anyhow!("--reasoning-effort is only supported with exec"));
        }
        if self.parsed.reasoning_effort.is_some() {
            return Err(anyhow!("--reasoning-effort was provided more than once"));
        }
        let value = args
            .next()
            .ok_or_else(|| anyhow!("--reasoning-effort requires a value"))?;
        let effort = ReasoningEffort::parse(&value).ok_or_else(|| {
            anyhow!("--reasoning-effort must be one of xsmall|small|medium|large|xlarge")
        })?;
        self.parsed.reasoning_effort = Some(effort);
        Ok(ArgParseFlow::Continue)
    }

    fn parse_extensions(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        if self.parsed.extensions.is_cli_set() {
            return Err(anyhow!("--extensions was provided more than once"));
        }
        let value = args
            .next()
            .ok_or_else(|| anyhow!("--extensions requires a value"))?;
        if value.is_empty() {
            return Err(anyhow!("--extensions requires a value"));
        }
        self.parsed.extensions = ExtensionSelection::from_cli(value);
        Ok(ArgParseFlow::Continue)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArgParseFlow {
    Continue,
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionExportOption {
    Limit,
    ScanLimit,
    AfterEventId,
    Kind,
}

fn validate_extension_args(parsed: &RawArgs) -> Result<()> {
    ensure_no_extensions(parsed, "extension")?;
    if parsed.provider_from_cli {
        return Err(anyhow!("--provider is not supported with extension"));
    }
    if parsed.model_from_cli {
        return Err(anyhow!("--model is not supported with extension"));
    }
    if parsed.auth_file_from_cli {
        return Err(anyhow!("--auth-file is not supported with extension"));
    }
    if parsed.provenance_from_cli {
        return Err(anyhow!("--provenance is not supported with extension"));
    }
    if parsed.no_tty {
        return Err(anyhow!("--no-tty is not supported with extension"));
    }
    Ok(())
}

fn parse_models_command(args: &mut impl Iterator<Item = String>) -> Result<ModelsCommand> {
    let mut command = ModelsCommand::List;
    let mut force = false;
    for arg in args.by_ref() {
        match arg.as_str() {
            "refresh" if command == ModelsCommand::List => {
                command = ModelsCommand::Refresh { force };
            }
            "refresh" => return Err(anyhow!("models refresh was provided more than once")),
            "--force" if matches!(command, ModelsCommand::Refresh { .. }) => {
                if force {
                    return Err(anyhow!("--force was provided more than once"));
                }
                force = true;
                command = ModelsCommand::Refresh { force };
            }
            "--force" => return Err(anyhow!("--force is only supported with models refresh")),
            "--provider" => return Err(anyhow!("--provider is not supported with models")),
            "--model" => return Err(anyhow!("--model is not supported with models")),
            "--provenance" => return Err(anyhow!("--provenance is not supported with models")),
            "--auth-file" => return Err(anyhow!("--auth-file is not supported with models")),
            "--no-tty" => return Err(anyhow!("--no-tty is not supported with models")),
            "--extensions" => return Err(anyhow!("--extensions is not supported with models")),
            "--replay" | "--resume" => {
                return Err(anyhow!("models cannot be used with --replay or --resume"));
            }
            _ => return Err(anyhow!("unknown models argument: {arg}")),
        }
    }
    Ok(command)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InteractiveLaunch {
    Tui,
    LineOriented,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TuiLaunchIntent {
    default_interactive: bool,
    no_tty_arg: bool,
    env_no_tty: bool,
    stdin_tty: bool,
    stdout_tty: bool,
}

fn decide_interactive_launch(intent: TuiLaunchIntent) -> InteractiveLaunch {
    if intent.default_interactive
        && !intent.no_tty_arg
        && !intent.env_no_tty
        && intent.stdin_tty
        && intent.stdout_tty
    {
        InteractiveLaunch::Tui
    } else {
        InteractiveLaunch::LineOriented
    }
}

fn euler_no_tty_env() -> bool {
    std::env::var("EULER_NO_TTY").is_ok_and(|value| value == "1")
}

fn build_run_args(
    parsed: &RawArgs,
    preference: Option<&ModelPreference>,
    catalog: &MergedModelCatalog,
    custom_providers: &ProviderConfigRegistry,
) -> Result<RunArgs> {
    let live = resolve_live_options(parsed, preference, custom_providers)?;
    let provider = provider_for_id(
        &live.provider_id,
        live.auth_file.clone(),
        &live.provider_options,
        custom_providers,
    )?;
    let model = match live.model {
        Some(model) => model,
        None => default_model_for_provider(&live.provider_id, catalog, custom_providers)?,
    };
    let observe = parsed.observe.clone().normalized()?;
    validate_observe_options(&observe)?;

    Ok(RunArgs {
        provider_id: live.provider_id,
        provider,
        model,
        model_catalog: catalog.clone(),
        auth_file: live.auth_file,
        custom_providers: custom_providers.clone(),
        max_output_tokens: parsed.max_output_tokens,
        max_tool_rounds: parsed.max_tool_rounds,
        auto_compaction: parsed.auto_compaction,
        compaction_budget_bytes: parsed.compaction_budget_bytes,
        reasoning_effort: parsed.reasoning_effort,
        extensions: parsed.extensions.clone(),
        observe,
        linefeed_history_insert: parsed.linefeed_history_insert.unwrap_or(parsed.tui),
        linefeed_history_insert_from_cli: parsed.linefeed_history_insert.is_some(),
    })
}

fn build_exec_args(
    parsed: &RawArgs,
    preference: Option<&ModelPreference>,
    catalog: &MergedModelCatalog,
    custom_providers: &ProviderConfigRegistry,
) -> Result<ExecArgs> {
    Ok(ExecArgs {
        run: build_run_args(parsed, preference, catalog, custom_providers)?,
        auto_approve: parsed.auto_approve.unwrap_or(AutoApproveTier::DEFAULT),
        prompt: (!parsed.exec_prompt.is_empty()).then(|| parsed.exec_prompt.join(" ")),
        resume_path: parsed.resume_path.clone(),
    })
}

fn build_login_args(parsed: &RawArgs) -> Result<LoginArgs> {
    ensure_no_provider_options(parsed, "login")?;
    ensure_no_extensions(parsed, "login")?;
    if !parsed.provider_from_cli {
        return Err(anyhow!("login requires explicit --provider chatgpt"));
    }
    if parsed.model_from_cli {
        return Err(anyhow!("--model is not supported with login"));
    }
    if parsed.provenance_from_cli {
        return Err(anyhow!("--provenance is not supported with login"));
    }
    login_args_for_provider(raw_known_provider_id(parsed)?, parsed.auth_file_from_cli)
}

fn build_logout_args(parsed: &RawArgs) -> Result<LogoutArgs> {
    ensure_no_provider_options(parsed, "logout")?;
    ensure_no_extensions(parsed, "logout")?;
    if !parsed.provider_from_cli {
        return Err(anyhow!("logout requires explicit --provider chatgpt"));
    }
    if parsed.model_from_cli {
        return Err(anyhow!("--model is not supported with logout"));
    }
    if parsed.provenance_from_cli {
        return Err(anyhow!("--provenance is not supported with logout"));
    }
    logout_args_for_provider(raw_known_provider_id(parsed)?, parsed.auth_file_from_cli)
}

fn build_auth_status_args(parsed: &RawArgs) -> Result<()> {
    ensure_no_provider_options(parsed, "auth status")?;
    ensure_no_extensions(parsed, "auth status")?;
    if parsed.provider_from_cli {
        return Err(anyhow!("--provider is not supported with auth status"));
    }
    if parsed.model_from_cli {
        return Err(anyhow!("--model is not supported with auth status"));
    }
    if parsed.provenance_from_cli {
        return Err(anyhow!("--provenance is not supported with auth status"));
    }
    if parsed.auth_file_from_cli {
        return Err(anyhow!("--auth-file is not supported with auth status"));
    }
    Ok(())
}

fn build_models_args(parsed: &RawArgs) -> Result<()> {
    ensure_no_provider_options(parsed, "models")?;
    ensure_no_extensions(parsed, "models")?;
    if parsed.provider_from_cli {
        return Err(anyhow!("--provider is not supported with models"));
    }
    if parsed.model_from_cli {
        return Err(anyhow!("--model is not supported with models"));
    }
    if parsed.provenance_from_cli {
        return Err(anyhow!("--provenance is not supported with models"));
    }
    if parsed.auth_file_from_cli {
        return Err(anyhow!("--auth-file is not supported with models"));
    }
    if parsed.no_tty {
        return Err(anyhow!("--no-tty is not supported with models"));
    }
    Ok(())
}

pub(crate) fn provider_for_id(
    provider_id: &str,
    auth_file: Option<PathBuf>,
    options: &ProviderOptions,
    custom_providers: &ProviderConfigRegistry,
) -> Result<Box<dyn ModelProvider>> {
    let provider_id = parse_provider_id(provider_id, custom_providers)?;
    if let Ok(provider_id) = parse_known_provider_id(&provider_id) {
        return Ok(match provider_id.as_str() {
            "fixture" => fixture_provider(options)?,
            "chatgpt" => match auth_file {
                Some(path) => {
                    reject_provider_options("chatgpt", options)?;
                    Box::new(ChatGptProvider::legacy_auth_file(path))
                }
                None => {
                    reject_provider_options("chatgpt", options)?;
                    Box::new(ChatGptProvider::stored_euler_auth(
                        StoredChatGptAuth::new_default(),
                    ))
                }
            },
            "anthropic" => {
                reject_provider_options("anthropic", options)?;
                Box::new(AnthropicProvider::with_api_key_auth(api_key_auth(
                    auth_file,
                )))
            }
            "openai" => {
                reject_provider_options("openai", options)?;
                Box::new(OpenAiProvider::with_api_key_auth(api_key_auth(auth_file)))
            }
            "openrouter" => {
                reject_provider_options("openrouter", options)?;
                Box::new(OpenRouterProvider::with_api_key_auth(api_key_auth(
                    auth_file,
                )))
            }
            "xai" => {
                reject_provider_options("xai", options)?;
                Box::new(XaiProvider::with_api_key_auth(api_key_auth(auth_file)))
            }
            other => return Err(anyhow!("provider `{other}` is missing CLI factory wiring")),
        });
    }
    if let Some(provider) = custom_providers.provider(&provider_id) {
        reject_provider_options(&provider_id, options)?;
        if auth_file.is_some() {
            return Err(anyhow!(
                "--auth-file is not supported with provider {provider_id}"
            ));
        }
        return Ok(Box::new(CustomOpenAiProvider::from_config(
            provider.clone(),
        )?));
    }
    Err(anyhow!("unknown provider: {provider_id}"))
}

fn api_key_auth(auth_file: Option<PathBuf>) -> StoredApiKeyAuth {
    auth_file
        .map(StoredApiKeyAuth::auth_file)
        .unwrap_or_else(StoredApiKeyAuth::new_default)
}

fn fixture_provider(options: &ProviderOptions) -> Result<Box<dyn ModelProvider>> {
    if let Some(key) = options.keys().find(|key| *key != "event-script") {
        return Err(anyhow!(
            "provider option `{key}` is not supported by provider fixture"
        ));
    }
    match options.values.get("event-script").map(String::as_str) {
        None => Ok(Box::new(EchoProvider)),
        Some("") => Err(anyhow!("provider option `event-script` requires a value")),
        Some(path) => Ok(Box::new(fixture_script::provider_from_event_script_path(
            path,
        )?)),
    }
}

fn reject_provider_options(provider: &str, options: &ProviderOptions) -> Result<()> {
    if let Some(key) = options.keys().next() {
        Err(anyhow!(
            "provider option `{key}` is not supported by provider {provider}"
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn resume_provider_set(
    original: &ModelTarget,
    active: &ModelTarget,
    auth_file: Option<PathBuf>,
) -> Result<ProviderSet> {
    resume_provider_set_with_custom(
        original,
        active,
        auth_file,
        &ProviderConfigRegistry::default(),
    )
}

fn resume_provider_set_with_custom(
    original: &ModelTarget,
    active: &ModelTarget,
    auth_file: Option<PathBuf>,
    custom_providers: &ProviderConfigRegistry,
) -> Result<ProviderSet> {
    let mut providers = ProviderSet::new();
    // Only require auth for the active target — the provider that will
    // actually make API calls. The original target is historical context
    // for fold seeding; requiring its auth would break valid sessions
    // that switched away from a provider the user no longer has creds for.
    let active_provider = provider_for_id(
        &active.provider,
        auth_file.clone(),
        &ProviderOptions::default(),
        custom_providers,
    )?;
    validate_provider_auth(&active.provider, auth_file.as_deref(), || {
        active_provider.validate_auth()
    })?;
    providers.insert_named(active.provider.clone(), active_provider);
    if original.provider != active.provider {
        // Best-effort: insert original provider without auth requirement
        // so fold history is representable, but don't fail if unavailable.
        if let Ok(original_provider) = provider_for_id(
            &original.provider,
            auth_file,
            &ProviderOptions::default(),
            custom_providers,
        ) {
            let _ = original_provider.validate_auth(); // warn-worthy but not fatal
            providers.insert_named(original.provider.clone(), original_provider);
        }
    }
    // A resumed session must be able to /model-switch to any configured
    // provider, exactly like a fresh TUI session (review v2 §14.5 — switches
    // were rejected with "provider is not configured"). Auth stays lazy:
    // invoking an un-credentialed provider still fails loudly at call time.
    fill_provider_set(&mut providers, custom_providers);
    Ok(providers)
}

/// Best-effort: add every builtin + custom provider not already present.
fn fill_provider_set(providers: &mut ProviderSet, custom_providers: &ProviderConfigRegistry) {
    for descriptor in BUILTIN_PROVIDERS {
        insert_provider_if_missing(providers, descriptor.id, custom_providers);
    }
    let mut custom_ids = custom_providers
        .providers()
        .map(|provider| provider.id.as_str())
        .collect::<Vec<_>>();
    custom_ids.sort_unstable();
    for provider_id in custom_ids {
        insert_provider_if_missing(providers, provider_id, custom_providers);
    }
}

fn insert_provider_if_missing(
    providers: &mut ProviderSet,
    provider_id: &str,
    custom_providers: &ProviderConfigRegistry,
) {
    if providers.contains(provider_id) {
        return;
    }
    let Ok(provider) = provider_for_id(
        provider_id,
        None,
        &ProviderOptions::default(),
        custom_providers,
    ) else {
        return;
    };
    providers.insert_named(provider_id.to_owned(), provider);
}

fn tui_provider_set(
    active_provider_id: String,
    active_provider: Box<dyn ModelProvider>,
    custom_providers: &ProviderConfigRegistry,
) -> ProviderSet {
    let mut providers = ProviderSet::new();
    providers.insert_named(active_provider_id, active_provider);
    fill_provider_set(&mut providers, custom_providers);
    providers
}

fn invocation_target(run: &RunArgs) -> ModelTarget {
    ModelTarget::new(run.provider_id.clone(), run.model.clone())
}

fn load_known_model_preference(preference_path: Option<&Path>) -> Option<ModelPreference> {
    let path = preference_path?;
    match model_preference::load_model_preference(path) {
        PreferenceLoad::Loaded(preference) => canonical_model_preference(preference),
        PreferenceLoad::Missing => None,
        PreferenceLoad::Ignored(message) => {
            eprintln!("warning: ignored model preference: {message}");
            None
        }
    }
}

fn load_known_theme_preference(preference_path: Option<&Path>) -> Option<ThemeChoice> {
    let path = preference_path?;
    match model_preference::load_theme_preference(path) {
        ThemePreferenceLoad::Loaded(theme) => ThemeChoice::parse(&theme),
        ThemePreferenceLoad::Missing => None,
        ThemePreferenceLoad::Ignored(message) => {
            eprintln!("warning: ignored theme preference: {message}");
            None
        }
    }
}

fn load_timestamps_preference(preference_path: Option<&Path>) -> Option<bool> {
    let path = preference_path?;
    match model_preference::load_timestamps_preference(path) {
        model_preference::TimestampsPreferenceLoad::Loaded(show) => Some(show),
        model_preference::TimestampsPreferenceLoad::Missing => None,
        model_preference::TimestampsPreferenceLoad::Ignored(message) => {
            eprintln!("warning: ignored timestamps preference: {message}");
            None
        }
    }
}

fn load_notifications_preference(preference_path: Option<&Path>) -> Option<bool> {
    let path = preference_path?;
    match model_preference::load_notifications_preference(path) {
        model_preference::NotificationsPreferenceLoad::Loaded(enabled) => Some(enabled),
        model_preference::NotificationsPreferenceLoad::Missing => None,
        model_preference::NotificationsPreferenceLoad::Ignored(message) => {
            eprintln!("warning: ignored notifications preference: {message}");
            None
        }
    }
}

fn load_known_model_catalog(model_catalog_path: Option<&Path>) -> MergedModelCatalog {
    let load = model_catalog::load_model_catalog(model_catalog_path);
    for warning in load.warnings {
        eprintln!("warning: ignored model catalog: {warning}");
    }
    load.catalog
}

fn load_custom_provider_config(provider_config_path: Option<&Path>) -> ProviderConfigRegistry {
    let load = provider_config_runtime::load_provider_config(provider_config_path);
    for warning in load.warnings {
        eprintln!("warning: ignored provider config: {warning}");
    }
    load.registry
}

#[derive(Default)]
struct EnvArgs {
    provider: Option<String>,
    model: Option<String>,
    auth_file: Option<PathBuf>,
}

struct CliDecider;

impl PermissionDecider for CliDecider {
    fn decide(&mut self, request: &PermissionRequest) -> DeciderVerdict {
        eprint!(
            "permission: allow {} for {}? [y/N] ",
            request.capability.as_str(),
            request.reason
        );
        let _ = io::stderr().flush();
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer).is_ok()
            && matches!(answer.trim(), "y" | "Y" | "yes" | "YES")
        {
            DeciderVerdict::Allow
        } else {
            DeciderVerdict::Deny
        }
    }
}

#[cfg(test)]
#[path = "main_exec_tests.rs"]
mod exec_tests;
#[cfg(test)]
#[path = "main_extension_tests.rs"]
mod extension_tests;
#[cfg(test)]
#[path = "main_session_export_tests.rs"]
mod session_export_tests;
#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
