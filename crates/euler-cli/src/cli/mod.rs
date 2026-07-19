mod args;
mod command;
pub(crate) mod extension_run;
mod model_resolution;
pub(crate) mod permission;
pub(crate) mod providers;
mod raw_args;
mod scrub;
mod session;
mod terminal;

#[cfg(test)]
pub(crate) use args::ProviderOptions;
pub(crate) use args::{ensure_no_extensions, RawArgs};
#[cfg(test)]
pub(crate) use model_resolution::{resolve_live_options, LiveOptions};
#[cfg(test)]
pub(crate) use scrub::parse_values as parse_scrub_values;
#[cfg(test)]
pub(crate) use session::{
    apply_interactive_tui_linefeed_default, non_empty_exec_prompt, validate_resume_live_target,
};
#[cfg(test)]
pub(crate) use terminal::{decide_interactive_launch, InteractiveLaunch, TuiLaunchIntent};

use anyhow::Result;
#[cfg(not(test))]
use command::{Args, Command, ModelsCommand};
#[cfg(test)]
pub(crate) use command::{Args, Command, ModelsCommand, ResumeLaunch};
#[cfg(test)]
pub(crate) use command::{ExecArgs, RunArgs};
#[cfg(test)]
pub(crate) use permission::EnvArgs;
use scrub::run as run_scrub;
use session::{resume_interactive_entry, run_exec, run_interactive_entry, run_tui};

use crate::auth_commands::{logout_chatgpt, print_auth_status};
use crate::extension_cli::run_extension_command;
use crate::login::login_chatgpt;
use crate::session_export::run_session_export;
use crate::session_lifecycle::resolve_resume_target;
use crate::ui::transcript::render_line_oriented;
use crate::{help, model_catalog, model_catalog_refresh, provider_config_runtime};
use euler_core::read_provenance;
use euler_event::EventKind;
use std::collections::BTreeSet;
use std::io;

pub(crate) fn run() -> Result<()> {
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
        Command::Resume { path, run, launch } => {
            resume_interactive_entry(resolve_resume_target(path)?, run, launch, args.no_tty)
        }
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
        Command::Models(ModelsCommand::Refresh) => model_catalog_refresh::refresh_model_catalog(
            model_catalog::default_model_catalog_path().as_deref(),
            io::stdout(),
            io::stderr(),
        ),
        Command::Extension(extension) => run_extension_command(extension),
        Command::Scrub(scrub) => run_scrub(scrub),
    }
}
