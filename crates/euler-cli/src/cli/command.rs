use super::args::{
    ensure_no_extensions, ensure_no_provider_options, linefeed_history_flag_label, RawArgs,
};
use super::model_resolution::{
    default_model_for_provider, raw_known_provider_id, resolve_live_options,
};
use super::permission::EnvArgs;
use super::providers::{
    load_custom_provider_config, load_known_model_catalog, load_known_model_preference,
    provider_for_id,
};
use super::scrub::ScrubArgs;
use anyhow::{anyhow, Result};
use euler_core::{CompactionTier, PermissionReviewer, ReasoningEffort};
use euler_provider::catalog::MergedModelCatalog;
use euler_provider::provider_config::ProviderConfigRegistry;
use euler_provider::ModelProvider;
use std::path::{Path, PathBuf};

use crate::auth_commands::{logout_args_for_provider, LogoutArgs};
use crate::bundled_extensions::ObserveOptions;
use crate::extension_cli::ExtensionArgs;
use crate::extension_enablement::ExtensionSelection;
use crate::login::{login_args_for_provider, LoginArgs};
use crate::model_preference::{self, ModelPreference};
use crate::session_export::{build_session_export_args, ProvenanceExportArgs};
use crate::session_lifecycle::LiveProvenance;
use crate::subagent::AutoApproveTier;
use crate::{model_catalog, provider_config_runtime};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModelsCommand {
    List,
    Refresh { force: bool },
}

pub(crate) struct Args {
    pub(crate) provenance_path: PathBuf,
    pub(crate) provenance_from_cli: bool,
    pub(crate) command: Command,
    pub(crate) default_interactive: bool,
    pub(crate) no_tty: bool,
}

impl Args {
    pub(crate) fn live_provenance(&self) -> LiveProvenance {
        if self.provenance_from_cli {
            LiveProvenance::Explicit(self.provenance_path.clone())
        } else {
            LiveProvenance::HomeSession
        }
    }
}

/// Replay reads an existing log and ignores live-provider arguments entirely,
/// including their validation.
pub(crate) enum Command {
    Replay {
        path: PathBuf,
    },
    Run(RunArgs),
    Tui(RunArgs),
    Exec(ExecArgs),
    Resume {
        path: PathBuf,
        run: RunArgs,
        launch: ResumeLaunch,
    },
    Login(LoginArgs),
    Logout(LogoutArgs),
    AuthStatus,
    Models(ModelsCommand),
    SessionExport(ProvenanceExportArgs),
    Extension(ExtensionArgs),
    Scrub(ScrubArgs),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResumeLaunch {
    /// Match bare interactive startup: TUI on a usable terminal, otherwise
    /// the line-oriented interface.
    Auto,
    LineOriented,
    Tui,
}

pub(crate) struct RunArgs {
    pub(crate) provider_id: String,
    pub(crate) provider: Box<dyn ModelProvider>,
    pub(crate) model: String,
    pub(crate) model_catalog: MergedModelCatalog,
    pub(crate) auth_file: Option<PathBuf>,
    pub(crate) custom_providers: ProviderConfigRegistry,
    pub(crate) max_output_tokens: Option<u64>,
    pub(crate) max_tool_rounds: Option<usize>,
    pub(crate) auto_compaction: Option<CompactionTier>,
    pub(crate) compaction_budget_bytes: Option<usize>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) permission_reviewer: Option<PermissionReviewer>,
    pub(crate) extensions: ExtensionSelection,
    pub(crate) observe: ObserveOptions,
    pub(crate) linefeed_history_insert: bool,
    pub(crate) linefeed_history_insert_from_cli: bool,
}
pub(crate) struct ExecArgs {
    pub(crate) run: RunArgs,
    pub(crate) auto_approve: AutoApproveTier,
    pub(crate) prompt: Option<String>,
    pub(crate) resume_path: Option<PathBuf>,
}
impl Args {
    pub(super) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
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
    pub(crate) fn parse_with_env(
        args: &mut impl Iterator<Item = String>,
        env: EnvArgs,
    ) -> Result<Self> {
        Self::parse_with_env_and_preference(args, env, None)
    }

    #[cfg(test)]
    pub(crate) fn parse_with_env_and_preference_path(
        args: &mut impl Iterator<Item = String>,
        env: EnvArgs,
        preference_path: &Path,
    ) -> Result<Self> {
        Self::parse_with_env_and_preference(args, env, Some(preference_path))
    }

    #[cfg(test)]
    pub(crate) fn parse_with_env_and_preference(
        args: &mut impl Iterator<Item = String>,
        env: EnvArgs,
        preference_path: Option<&Path>,
    ) -> Result<Self> {
        Self::parse_with_env_preference_and_catalog(args, env, preference_path, None)
    }

    #[cfg(test)]
    pub(crate) fn parse_with_env_preference_and_catalog_path(
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
    pub(crate) fn parse_with_env_preference_and_catalog(
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

    pub(crate) fn parse_with_env_preference_catalog_and_provider_config(
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
        let command = build_command_from_parsed(
            &parsed,
            CatalogPaths {
                preference: preference_path,
                model_catalog: model_catalog_path,
                provider_config: provider_config_path,
            },
            &mut default_interactive,
        )?;

        Ok(Self {
            provenance_path: parsed.provenance_path,
            provenance_from_cli: parsed.provenance_from_cli,
            command,
            default_interactive,
            no_tty: parsed.no_tty,
        })
    }
}

/// The three optional on-disk config paths threaded into command building.
#[derive(Clone, Copy)]
struct CatalogPaths<'a> {
    preference: Option<&'a Path>,
    model_catalog: Option<&'a Path>,
    provider_config: Option<&'a Path>,
}

/// Resolve the parsed flags into a single [`Command`], setting
/// `default_interactive` when the bare (no explicit `run`) interactive path is
/// taken. Extracted from the parser so each stays within the line budget.
fn build_command_from_parsed(
    parsed: &RawArgs,
    paths: CatalogPaths<'_>,
    default_interactive: &mut bool,
) -> Result<Command> {
    let command = if parsed.login {
        Command::Login(build_login_args(parsed)?)
    } else if parsed.logout {
        Command::Logout(build_logout_args(parsed)?)
    } else if parsed.auth_status {
        build_auth_status_args(parsed)?;
        Command::AuthStatus
    } else if parsed.models {
        build_models_args(parsed)?;
        Command::Models(parsed.models_command)
    } else if parsed.exec {
        let preference = load_known_model_preference(paths.preference);
        let model_catalog = load_known_model_catalog(paths.model_catalog);
        let custom_providers = load_custom_provider_config(paths.provider_config);
        Command::Exec(build_exec_args(
            parsed,
            preference.as_ref(),
            &model_catalog,
            &custom_providers,
        )?)
    } else if parsed.session_export.is_active() {
        ensure_no_provider_options(parsed, "session-export")?;
        Command::SessionExport(build_session_export_args(parsed)?)
    } else if let Some(extension) = parsed.extension.as_ref() {
        ensure_no_provider_options(parsed, "extension")?;
        validate_extension_args(parsed)?;
        Command::Extension(extension.clone())
    } else if let Some(scrub) = parsed.scrub.as_ref() {
        ensure_no_provider_options(parsed, "scrub")?;
        Command::Scrub(scrub.clone())
    } else if parsed.tui {
        build_tui_command(parsed, paths)?
    } else if let Some(path) = parsed.replay_path.clone() {
        ensure_no_provider_options(parsed, "--replay")?;
        ensure_no_extensions(parsed, "--replay")?;
        Command::Replay { path }
    } else if let Some(path) = parsed.resume_path.clone() {
        ensure_no_provider_options(parsed, "--resume")?;
        let model_catalog = load_known_model_catalog(paths.model_catalog);
        let custom_providers = load_custom_provider_config(paths.provider_config);
        Command::Resume {
            path,
            run: build_run_args(parsed, None, &model_catalog, &custom_providers)?,
            launch: if parsed.explicit_run {
                ResumeLaunch::LineOriented
            } else {
                ResumeLaunch::Auto
            },
        }
    } else {
        let preference = load_known_model_preference(paths.preference);
        let model_catalog = load_known_model_catalog(paths.model_catalog);
        let custom_providers = load_custom_provider_config(paths.provider_config);
        *default_interactive = !parsed.explicit_run;
        Command::Run(build_run_args(
            parsed,
            preference.as_ref(),
            &model_catalog,
            &custom_providers,
        )?)
    };
    Ok(command)
}

fn build_tui_command(parsed: &RawArgs, paths: CatalogPaths<'_>) -> Result<Command> {
    if parsed.no_tty {
        return Err(anyhow!("tui cannot be combined with --no-tty"));
    }
    let preference = load_known_model_preference(paths.preference);
    let model_catalog = load_known_model_catalog(paths.model_catalog);
    let custom_providers = load_custom_provider_config(paths.provider_config);
    let run = build_run_args(
        parsed,
        preference.as_ref(),
        &model_catalog,
        &custom_providers,
    )?;
    let Some(path) = parsed.resume_path.clone() else {
        return Ok(Command::Tui(run));
    };
    ensure_no_provider_options(parsed, "--resume")?;
    Ok(Command::Resume {
        path,
        run,
        launch: ResumeLaunch::Tui,
    })
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

pub(super) fn parse_models_command(
    args: &mut impl Iterator<Item = String>,
) -> Result<ModelsCommand> {
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
        permission_reviewer: parsed.permission_reviewer,
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
