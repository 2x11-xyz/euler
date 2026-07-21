use crate::extension_cli::resolve_round_observer;
use crate::{extension_cli, model_preference};
use anyhow::Result;
use auth_commands::LogoutArgs;
use cli::permission::CliDecider;
use cli::providers::{
    load_custom_provider_config, load_known_theme_preference, provider_for_id, resume_provider_set,
    resume_provider_set_with_custom, tui_provider_set,
};
use cli::{
    apply_interactive_tui_linefeed_default, decide_interactive_launch, non_empty_exec_prompt,
    parse_scrub_values, resolve_live_options, validate_resume_live_target, Args, Command, EnvArgs,
    ExecArgs, InteractiveLaunch, LiveOptions, ModelsCommand, ProviderOptions, RawArgs,
    ResumeLaunch, RunArgs, TuiLaunchIntent,
};
use companion_run::execute_headless_companion_run;
use euler_core::{CompactionTier, ModelTarget, PermissionRequest, ProvenanceWriter, Session};
use euler_event::EventKind;
use euler_provider::catalog::{
    MergedModelCatalog, BUILTIN_PROVIDERS, DEFAULT_ANTHROPIC_MODEL, DEFAULT_CHATGPT_MODEL,
    DEFAULT_FIXTURE_MODEL, DEFAULT_OPENAI_MODEL, DEFAULT_OPENROUTER_MODEL, DEFAULT_XAI_MODEL,
};
use euler_provider::provider_config::ProviderConfigRegistry;
use euler_provider::ReasoningEffort;
use login::LoginArgs;
use session_export::{execute_session_export, ProvenanceExportArgs};
use session_lifecycle::{apply_catalog_context_limit, session_config, LiveProvenance};
use std::path::PathBuf;
use subagent::{AutoApproveTier, SubagentDecider};
use theme_catalog::ThemeChoice;

use crate::{
    auth_commands, cli, companion_run, login, session_export, session_lifecycle, subagent,
    theme_catalog,
};

#[path = "main_exec_tests.rs"]
mod exec_tests;
#[path = "main_extension_tests.rs"]
mod extension_tests;
#[path = "main_session_export_tests.rs"]
mod session_export_tests;
#[path = "main_tests.rs"]
mod tests;
