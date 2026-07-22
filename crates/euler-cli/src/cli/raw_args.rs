use super::args::{
    accept_linefeed_history_option, accept_top_level_command, parse_positive_u64, set_once,
    ArgParseFlow, ProviderOptions, RawArgs, TopLevelCommand,
    EXPERIMENTAL_TUI_LINEFEED_HISTORY_FLAG, NO_TUI_LINEFEED_HISTORY_FLAG,
};
use super::command::{parse_models_command, ModelsCommand};
use super::permission::EnvArgs;
use anyhow::{anyhow, Result};
use euler_core::{CompactionTier, PermissionReviewer, ReasoningEffort};
use std::num::NonZeroU64;
use std::path::PathBuf;

use super::scrub::ScrubArgs;
use crate::extension_cli::ExtensionArgs;
use crate::extension_cli::ObserveOptions;
use crate::extension_enablement::ExtensionSelection;
use crate::subagent::AutoApproveTier;

pub(super) struct RawArgsParser {
    parsed: RawArgs,
    top_level_command: Option<TopLevelCommand>,
    saw_any_arg: bool,
}

impl RawArgsParser {
    pub(super) fn new(env: EnvArgs) -> Self {
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
                permission_reviewer: None,
                extensions: ExtensionSelection::default(),
                observe: ObserveOptions::default(),
                login: false,
                logout: false,
                auth_status: false,
                models: false,
                models_command: ModelsCommand::List,
                extension: None,
                scrub: None,
                no_tty: false,
                linefeed_history_insert: None,
                project_context: None,
                accept_relocation: false,
            },
            top_level_command: None,
            saw_any_arg: false,
        }
    }

    pub(super) fn parse(mut self, args: &mut impl Iterator<Item = String>) -> Result<RawArgs> {
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
            "extension" if !self.parsed.exec => self.parse_extension_command(args),
            "scrub" if !self.parsed.exec => self.parse_scrub_command(args),
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
            "--auto-approve" => self.parse_auto_approve(args),
            "--max-output-tokens" => self.parse_max_output_tokens(args),
            "--max-tool-rounds" => self.parse_max_tool_rounds(args),
            "--auto-compaction" => self.parse_auto_compaction(args),
            "--compaction-budget-bytes" => self.parse_compaction_budget_bytes(args),
            "--reasoning-effort" => self.parse_reasoning_effort(args),
            "--permission-reviewer" => self.parse_permission_reviewer(args),
            "--project-context" => self.parse_project_context(args),
            "--accept-relocation" => {
                self.parsed.accept_relocation = true;
                Ok(ArgParseFlow::Continue)
            }
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

    fn parse_extension_command(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        accept_top_level_command(&mut self.top_level_command, TopLevelCommand::Extension)?;
        self.parsed.extension = Some(ExtensionArgs::parse(args)?);
        Ok(ArgParseFlow::Stop)
    }

    fn parse_scrub_command(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        accept_top_level_command(&mut self.top_level_command, TopLevelCommand::Scrub)?;
        self.parsed.scrub = Some(ScrubArgs::parse(args)?);
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
        set_once(&mut self.parsed.observe.extension_id, "--observe", || {
            args.next()
                .ok_or_else(|| anyhow!("--observe requires an extension id"))
        })?;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_observe_cadence(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        set_once(
            &mut self.parsed.observe.cadence_rounds,
            "--observe-cadence",
            || {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--observe-cadence requires a value"))?;
                Ok(
                    NonZeroU64::new(parse_positive_u64(&value, "--observe-cadence")?)
                        .expect("positive cadence is non-zero"),
                )
            },
        )?;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_linefeed_history(&mut self, enabled: bool, arg: &str) -> Result<ArgParseFlow> {
        accept_linefeed_history_option(&mut self.parsed.linefeed_history_insert, enabled, arg)?;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_auto_approve(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        if !self.parsed.exec {
            return Err(anyhow!("--auto-approve is only supported with exec"));
        }
        set_once(&mut self.parsed.auto_approve, "--auto-approve", || {
            let value = args
                .next()
                .ok_or_else(|| anyhow!("--auto-approve requires a tier"))?;
            AutoApproveTier::parse(&value).ok_or_else(|| {
                anyhow!(
                    "unknown auto-approve tier: {value}; supported tiers: {}",
                    AutoApproveTier::SUPPORTED
                )
            })
        })?;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_max_output_tokens(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        if !self.parsed.exec {
            return Err(anyhow!("--max-output-tokens is only supported with exec"));
        }
        set_once(
            &mut self.parsed.max_output_tokens,
            "--max-output-tokens",
            || {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--max-output-tokens requires a value"))?;
                parse_positive_u64(&value, "--max-output-tokens")
            },
        )?;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_max_tool_rounds(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        if !self.parsed.exec {
            return Err(anyhow!("--max-tool-rounds is only supported with exec"));
        }
        set_once(
            &mut self.parsed.max_tool_rounds,
            "--max-tool-rounds",
            || {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--max-tool-rounds requires a value"))?;
                Ok(parse_positive_u64(&value, "--max-tool-rounds")? as usize)
            },
        )?;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_auto_compaction(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        if !self.parsed.exec {
            return Err(anyhow!("--auto-compaction is only supported with exec"));
        }
        set_once(
            &mut self.parsed.auto_compaction,
            "--auto-compaction",
            || {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--auto-compaction requires a value"))?;
                CompactionTier::parse(&value)
                    .ok_or_else(|| anyhow!("--auto-compaction must be one of off|stubs"))
            },
        )?;
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
        set_once(
            &mut self.parsed.compaction_budget_bytes,
            "--compaction-budget-bytes",
            || {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--compaction-budget-bytes requires a value"))?;
                Ok(parse_positive_u64(&value, "--compaction-budget-bytes")? as usize)
            },
        )?;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_reasoning_effort(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        if !self.parsed.exec {
            return Err(anyhow!("--reasoning-effort is only supported with exec"));
        }
        set_once(
            &mut self.parsed.reasoning_effort,
            "--reasoning-effort",
            || {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--reasoning-effort requires a value"))?;
                ReasoningEffort::parse(&value).ok_or_else(|| {
                    anyhow!(
                        "--reasoning-effort must be one of xsmall|small|medium|large|xlarge|max"
                    )
                })
            },
        )?;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_permission_reviewer(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        set_once(
            &mut self.parsed.permission_reviewer,
            "--permission-reviewer",
            || {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--permission-reviewer requires a value"))?;
                PermissionReviewer::parse(&value)
                    .ok_or_else(|| anyhow!("--permission-reviewer must be one of user|guardian"))
            },
        )?;
        Ok(ArgParseFlow::Continue)
    }

    fn parse_project_context(
        &mut self,
        args: &mut impl Iterator<Item = String>,
    ) -> Result<ArgParseFlow> {
        set_once(
            &mut self.parsed.project_context,
            "--project-context",
            || {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--project-context requires a value"))?;
                euler_core::ProjectContextPolicy::parse(&value).ok_or_else(|| {
                    anyhow!(
                        "--project-context must be one of {}",
                        euler_core::ProjectContextPolicy::SUPPORTED
                    )
                })
            },
        )?;
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
