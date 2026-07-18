use super::command::ModelsCommand;
use super::permission::EnvArgs;
use super::scrub::ScrubArgs;
use anyhow::{anyhow, Result};
use euler_core::{CompactionTier, PermissionReviewer, ReasoningEffort};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::bundled_extensions::ObserveOptions;
use crate::extension_cli::ExtensionArgs;
use crate::extension_enablement::ExtensionSelection;
use crate::session_export::RawProvenanceExportArgs;
use crate::subagent::AutoApproveTier;

pub(super) const EXPERIMENTAL_TUI_LINEFEED_HISTORY_FLAG: &str =
    "--experimental-tui-linefeed-history";
pub(super) const NO_TUI_LINEFEED_HISTORY_FLAG: &str = "--no-tui-linefeed-history";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TopLevelCommand {
    Run,
    Tui,
    Exec,
    Login,
    Logout,
    AuthStatus,
    Models,
    SessionExport,
    Extension,
    Scrub,
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
            Self::Scrub => "scrub",
        }
    }
}

pub(super) fn accept_top_level_command(
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

pub(super) fn linefeed_history_flag_label(enabled: bool) -> &'static str {
    if enabled {
        EXPERIMENTAL_TUI_LINEFEED_HISTORY_FLAG
    } else {
        NO_TUI_LINEFEED_HISTORY_FLAG
    }
}

pub(super) fn accept_linefeed_history_option(
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

pub(super) fn parse_positive_u64(value: &str, flag: &str) -> Result<u64> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| anyhow!("{flag} requires a positive integer"))?;
    if parsed == 0 {
        Err(anyhow!("{flag} requires a positive integer"))
    } else {
        Ok(parsed)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProviderOptions {
    pub(crate) values: BTreeMap<String, String>,
}

impl ProviderOptions {
    pub(crate) fn insert(&mut self, option: &str) -> Result<()> {
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

    pub(super) fn keys(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }
}

pub(super) fn ensure_no_provider_options(parsed: &RawArgs, context: &str) -> Result<()> {
    if parsed.provider_options.values.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("--provider-option is not supported with {context}"))
    }
}

pub(crate) fn ensure_no_extensions(parsed: &RawArgs, context: &str) -> Result<()> {
    if parsed.extensions.is_cli_set() {
        Err(anyhow!("--extensions is not supported with {context}"))
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RawArgs {
    pub(crate) provenance_path: PathBuf,
    pub(crate) provenance_from_cli: bool,
    pub(crate) provider: Option<String>,
    pub(crate) provider_from_cli: bool,
    pub(crate) model: Option<String>,
    pub(crate) model_from_cli: bool,
    pub(crate) provider_options: ProviderOptions,
    pub(crate) auth_file: Option<PathBuf>,
    pub(crate) auth_file_from_cli: bool,
    pub(crate) replay_path: Option<PathBuf>,
    pub(crate) resume_path: Option<PathBuf>,
    pub(crate) explicit_run: bool,
    pub(crate) tui: bool,
    pub(crate) exec: bool,
    pub(crate) exec_prompt: Vec<String>,
    pub(crate) auto_approve: Option<AutoApproveTier>,
    pub(crate) max_output_tokens: Option<u64>,
    pub(crate) max_tool_rounds: Option<usize>,
    pub(crate) auto_compaction: Option<CompactionTier>,
    pub(crate) compaction_budget_bytes: Option<usize>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) permission_reviewer: Option<PermissionReviewer>,
    pub(crate) extensions: ExtensionSelection,
    pub(crate) observe: ObserveOptions,
    pub(crate) login: bool,
    pub(crate) logout: bool,
    pub(crate) auth_status: bool,
    pub(crate) models: bool,
    pub(crate) models_command: ModelsCommand,
    pub(crate) session_export: RawProvenanceExportArgs,
    pub(crate) extension: Option<ExtensionArgs>,
    pub(crate) scrub: Option<ScrubArgs>,
    pub(crate) no_tty: bool,
    pub(crate) linefeed_history_insert: Option<bool>,
}

impl RawArgs {
    pub(crate) fn parse_with_env(
        args: &mut impl Iterator<Item = String>,
        env: EnvArgs,
    ) -> Result<Self> {
        super::raw_args::RawArgsParser::new(env).parse(args)
    }

    pub(super) fn allows_tui_linefeed_history_option(&self) -> bool {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ArgParseFlow {
    Continue,
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SessionExportOption {
    Limit,
    ScanLimit,
    AfterEventId,
    Kind,
}
