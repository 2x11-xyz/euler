#![cfg_attr(test, allow(clippy::too_many_lines))] // unit-test exemption for inline test modules

mod auth_commands;
mod auth_validation;
mod bundled_extensions;
mod cli;
mod code_swarm_config;
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
mod offline_extension_runner;
mod provider_config_runtime;
mod session_export;
mod session_lifecycle;
mod subagent;
mod theme_catalog;
mod ui;

#[cfg(test)]
static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn main() -> anyhow::Result<()> {
    cli::run()
}

#[cfg(test)]
mod cli_tests;
