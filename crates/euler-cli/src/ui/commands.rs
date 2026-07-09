use super::theme::ThemeChoice;
use euler_core::{ActiveGrant, ApprovalMode, GrantSource, ReasoningEffort};
use euler_sdk::Capability;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub token: &'static str,
    pub summary: &'static str,
    pub args: &'static str,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandContext {
    pub model_choices: Vec<ModelChoice>,
    pub effort_choices: Vec<EffortChoice>,
    pub theme_choices: Vec<ThemeChoiceItem>,
    pub resume_items: Vec<ResumeItem>,
    pub checkpoint_items: Vec<CheckpointItem>,
    /// Bundled + linked extensions for the `/extension` manager and palette.
    pub extension_items: Vec<ExtensionManagerItem>,
    /// Extension slash entries (⋄ annotated in the palette).
    pub extension_slash_commands: Vec<ExtensionSlashCommand>,
    /// Saved `/code-swarm` reviewer model set (provider::model strings).
    pub code_swarm_models: Vec<String>,
}

/// One extension row for the `/extension` manager picker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionManagerItem {
    pub id: String,
    pub display_name: String,
    pub enabled: bool,
    pub bundled: bool,
    pub materialization: Option<String>,
    pub version: String,
    pub commands: Vec<String>,
    pub capabilities: Vec<String>,
    pub audit_status: Option<String>,
}

impl ExtensionManagerItem {
    pub fn label(&self) -> String {
        let mark = if self.enabled { "●" } else { "○" };
        let kind = if self.bundled {
            "bundled"
        } else {
            self.materialization.as_deref().unwrap_or("linked")
        };
        format!("{mark} {}  ({kind})", self.id)
    }

    pub fn details_text(&self) -> String {
        let mut lines = vec![
            format!("{}  v{}", self.display_name, self.version),
            format!("id: {}", self.id),
            format!(
                "state: {}",
                if self.enabled { "enabled" } else { "disabled" }
            ),
            format!(
                "source: {}",
                if self.bundled {
                    "bundled".to_owned()
                } else {
                    self.materialization
                        .clone()
                        .unwrap_or_else(|| "linked".to_owned())
                }
            ),
        ];
        if !self.commands.is_empty() {
            lines.push(format!("commands: {}", self.commands.join(", ")));
        }
        if !self.capabilities.is_empty() {
            lines.push(format!("capabilities: {}", self.capabilities.join(", ")));
        }
        if let Some(audit) = &self.audit_status {
            lines.push(format!("audit: {audit}"));
        }
        if self.bundled {
            lines.push("bundled extensions can be toggled but not removed".to_owned());
        }
        lines.join("\n")
    }
}

/// Extension-provided slash command surfaced in the palette under EXTENSIONS.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionSlashCommand {
    pub token: String,
    pub summary: String,
    pub extension_id: String,
    pub command: String,
    pub enabled: bool,
}

/// Palette row: core static command or extension-sourced entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaletteEntry {
    pub token: String,
    pub summary: String,
    pub args: String,
    pub kind: PaletteEntryKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PaletteEntryKind {
    Core,
    Extension {
        extension_id: String,
        command: String,
        enabled: bool,
    },
}

impl PaletteEntry {
    pub fn from_core(spec: CommandSpec) -> Self {
        Self {
            token: spec.token.to_owned(),
            summary: spec.summary.to_owned(),
            args: command_args(&spec),
            kind: PaletteEntryKind::Core,
        }
    }

    pub fn from_extension(cmd: &ExtensionSlashCommand) -> Self {
        Self {
            token: cmd.token.clone(),
            summary: cmd.summary.clone(),
            args: String::new(),
            kind: PaletteEntryKind::Extension {
                extension_id: cmd.extension_id.clone(),
                command: cmd.command.clone(),
                enabled: cmd.enabled,
            },
        }
    }

    pub fn is_extension(&self) -> bool {
        matches!(self.kind, PaletteEntryKind::Extension { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelChoice {
    pub provider: String,
    pub model: String,
    pub label: String,
    pub current: bool,
}

impl ModelChoice {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        let provider = provider.into();
        let model = model.into();
        Self {
            label: format!("{provider}::{model}"),
            provider,
            model,
            current: false,
        }
    }

    pub fn with_metadata(
        provider: impl Into<String>,
        model: impl Into<String>,
        context_window_tokens: Option<u64>,
        supports_reasoning: Option<bool>,
    ) -> Self {
        let mut choice = Self::new(provider, model);
        if let Some(suffix) = model_label_suffix(context_window_tokens, supports_reasoning) {
            choice.label.push_str(" — ");
            choice.label.push_str(&suffix);
        }
        choice
    }

    pub fn current(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            current: true,
            ..Self::new(provider, model)
        }
    }
}

fn model_label_suffix(
    context_window_tokens: Option<u64>,
    supports_reasoning: Option<bool>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(tokens) = context_window_tokens {
        parts.push(format!("{} ctx", format_context_tokens(tokens)));
    }
    if supports_reasoning == Some(true) {
        parts.push("reasoning".to_owned());
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn format_context_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        if tokens.is_multiple_of(1_000_000) {
            format!("{}M", tokens / 1_000_000)
        } else {
            format!("{:.2}M", tokens as f64 / 1_000_000.0)
        }
    } else if tokens >= 1_000 {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffortChoice {
    pub effort: ReasoningEffort,
    pub label: String,
    pub current: bool,
}

impl EffortChoice {
    pub fn new(effort: ReasoningEffort, current: ReasoningEffort) -> Self {
        let label = match effort {
            ReasoningEffort::XSmall => "xsmall - fastest/least reasoning",
            ReasoningEffort::Small => "small - light reasoning",
            ReasoningEffort::Medium => "medium - balanced default",
            ReasoningEffort::Large => "large - deeper reasoning",
            ReasoningEffort::XLarge => "xlarge - maximum reasoning",
        };
        Self {
            effort,
            label: label.to_owned(),
            current: effort == current,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThemeChoiceItem {
    pub choice: ThemeChoice,
    pub label: String,
    pub current: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumeItem {
    pub id: String,
    pub label: String,
    pub preview: Option<String>,
    pub status: Option<String>,
    pub group: Option<String>,
}

impl ResumeItem {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            preview: None,
            status: None,
            group: None,
        }
    }
}

/// One restorable workspace checkpoint for the `/rollback` picker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointItem {
    pub event_id: String,
    pub action: String,
    pub path: String,
    pub time: String,
}

impl CheckpointItem {
    pub fn new(
        event_id: impl Into<String>,
        action: impl Into<String>,
        path: impl Into<String>,
        time: impl Into<String>,
    ) -> Self {
        Self {
            event_id: event_id.into(),
            action: action.into(),
            path: path.into(),
            time: time.into(),
        }
    }

    pub fn label(&self) -> String {
        format!(
            "{}  {}  {}  {}",
            short_event_id(&self.event_id),
            self.action,
            self.path,
            short_time(&self.time)
        )
    }
}

fn short_event_id(event_id: &str) -> String {
    if event_id.len() <= 10 {
        event_id.to_owned()
    } else {
        event_id[event_id.len().saturating_sub(8)..].to_owned()
    }
}

fn short_time(ts: &str) -> String {
    // RFC3339 millis: keep HH:MM:SS when present.
    if let Some(t_idx) = ts.find('T') {
        let time = &ts[t_idx + 1..];
        if time.len() >= 8 {
            return time[..8].to_owned();
        }
    }
    ts.to_owned()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandAction {
    NewSession,
    SwitchModel {
        provider: String,
        model: String,
    },
    SetReasoningEffort {
        effort: ReasoningEffort,
    },
    NameSession {
        name: String,
    },
    CompactSession,
    ShowCompaction,
    ExportSession {
        path: Option<String>,
    },
    ExtensionRun {
        id: String,
        command: String,
        input: serde_json::Value,
        /// `--flag value…` text typed after the token; parsed against the
        /// command's ArgSpec at resolve time (where the descriptor lives).
        /// Mutually exclusive with a non-empty `input`.
        raw_args: Option<String>,
    },
    CompanionRun {
        input: serde_json::Value,
    },
    /// Persist the `/code-swarm` reviewer model set (1-5 provider::model).
    CodeSwarmSaveModels {
        models: Vec<String>,
    },
    /// Run the reviewer swarm: brief -> spawn companions -> report.
    CodeSwarmReview {
        prompt: Option<String>,
        personas: Option<Vec<String>>,
    },
    ShowStatus,
    Login {
        provider: String,
    },
    Logout {
        provider: String,
    },
    SetTheme {
        choice: ThemeChoice,
    },
    SetPermissionMode {
        capability: Capability,
        mode: ApprovalMode,
    },
    /// Open the permissions picker with live session/project grants.
    OpenPermissions,
    RevokeGrant {
        capability: Capability,
        pattern: String,
        source: GrantSource,
    },
    ResumeSession {
        session_id: String,
    },
    RollbackCheckpoint {
        event_id: String,
    },
    ShowHelp {
        text: String,
    },
    Quit,
    ScrollViewportToBottom,
    CopyLastAssistantResponse,
    ToggleTimestamps,
    /// Session-attributed aggregate diff (files this session touched).
    ShowDiff,
    /// Token breakdown for the session (costs only when catalog prices exist).
    ShowUsage,
    /// Dispatch to `causal-dag.export` (teach when extension disabled).
    DagExport,
    /// Open the extensions manager picker.
    OpenExtensionManager,
    /// Toggle extension enablement (registry + live session set).
    ExtensionToggle {
        id: String,
        enable: bool,
    },
    /// Show extension details in the transcript.
    ExtensionDetails {
        id: String,
    },
    /// Remove a non-bundled extension (unlink/uninstall).
    ExtensionRemove {
        id: String,
    },
    /// Validate → link → install → audit → enable a local package path.
    ExtensionAdd {
        path: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandEffect {
    Action(CommandAction),
    OpenPicker(PickerSpec),
    Message(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerSpec {
    Model(Vec<ModelChoice>),
    Effort(Vec<EffortChoice>),
    Theme(Vec<ThemeChoiceItem>),
    Permissions(Vec<PermissionChoice>),
    Resume(Vec<ResumeItem>),
    Rollback(Vec<CheckpointItem>),
    Extensions(Vec<ExtensionManagerItem>),
    /// `/code-swarm` reviewer-model checklist: selection IS the agent count.
    CodeSwarmModels {
        choices: Vec<ModelChoice>,
        selected: Vec<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermissionChoice {
    SetMode {
        capability: Capability,
        mode: ApprovalMode,
        label: String,
    },
    Revoke {
        capability: Capability,
        pattern: String,
        source: GrantSource,
        label: String,
    },
}

impl PermissionChoice {
    pub fn label(&self) -> &str {
        match self {
            Self::SetMode { label, .. } | Self::Revoke { label, .. } => label,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedCommand<'a> {
    pub token: &'a str,
    pub arg: Option<&'a str>,
}

const COMMAND_TABLE: &[CommandSpec] = &[
    CommandSpec {
        token: "/model",
        summary: "switch provider/model",
        args: "[provider::model]",
    },
    CommandSpec {
        token: "/new",
        summary: "start a fresh session",
        args: "",
    },
    CommandSpec {
        token: "/effort",
        summary: "set reasoning effort",
        args: "[level]",
    },
    CommandSpec {
        token: "/theme",
        summary: "switch theme",
        args: "",
    },
    CommandSpec {
        token: "/compact",
        summary: "compact eligible history",
        args: "",
    },
    CommandSpec {
        token: "/compaction",
        summary: "show canvas retention status",
        args: "",
    },
    CommandSpec {
        token: "/export",
        summary: "export this session",
        args: "[path]",
    },
    CommandSpec {
        token: "/extension",
        summary: "manage extensions or run a command",
        args: "[run <ext>.<cmd> [json-input]]",
    },
    CommandSpec {
        token: "/diff",
        summary: "session file changes (aggregate diff)",
        args: "",
    },
    CommandSpec {
        token: "/usage",
        summary: "token usage for this session",
        args: "",
    },
    CommandSpec {
        token: "/dag",
        summary: "export causal DAG for this session",
        args: "",
    },
    CommandSpec {
        token: "/companion",
        summary: "run a companion task",
        args: "run <json-agent-task>",
    },
    CommandSpec {
        token: "/status",
        summary: "show session status",
        args: "",
    },
    CommandSpec {
        token: "/hotkeys",
        summary: "show keyboard shortcuts",
        args: "",
    },
    CommandSpec {
        token: "/login",
        summary: "show login instructions",
        args: "[provider]",
    },
    CommandSpec {
        token: "/logout",
        summary: "show logout instructions",
        args: "[provider]",
    },
    CommandSpec {
        token: "/name",
        summary: "name this session",
        args: "<name>",
    },
    CommandSpec {
        token: "/permissions",
        summary: "approval modes and active grants",
        args: "",
    },
    CommandSpec {
        token: "/resume",
        summary: "resume a prior session",
        args: "",
    },
    CommandSpec {
        token: "/rollback",
        summary: "restore a workspace file from a checkpoint",
        args: "",
    },
    CommandSpec {
        token: "/help",
        summary: "show slash commands",
        args: "",
    },
    CommandSpec {
        token: "/quit",
        summary: "quit Euler",
        args: "",
    },
    CommandSpec {
        token: "/copy",
        summary: "copy last assistant response",
        args: "",
    },
    CommandSpec {
        token: "/timestamps",
        summary: "toggle timestamp gutter",
        args: "",
    },
];

pub fn command_table() -> &'static [CommandSpec] {
    COMMAND_TABLE
}

pub fn filter_commands(input: &str) -> Vec<CommandSpec> {
    let needle = filter_needle(input);
    command_table()
        .iter()
        .copied()
        .filter(|spec| command_matches(spec, &needle))
        .collect()
}

/// Core + extension palette entries, filtered by the first token of `input`.
/// Unfiltered lists put extension rows after core under an EXTENSIONS group
/// (rendered by the palette, not as a selectable entry).
pub fn filter_palette_entries(input: &str, context: &CommandContext) -> Vec<PaletteEntry> {
    let needle = filter_needle(input);
    let mut entries: Vec<PaletteEntry> = command_table()
        .iter()
        .copied()
        .filter(|spec| command_matches(spec, &needle))
        .map(PaletteEntry::from_core)
        .collect();
    for cmd in &context.extension_slash_commands {
        if palette_token_matches(&cmd.token, &needle) {
            entries.push(PaletteEntry::from_extension(cmd));
        }
    }
    entries
}

fn palette_token_matches(token: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let token = token.trim_start_matches('/').to_lowercase();
    token.starts_with(needle) || token.contains(needle)
}

/// Build extension slash entries: short token when free, else `/<ext>.<cmd>`.
/// Core always wins collisions.
pub fn build_extension_slash_commands(
    items: &[ExtensionManagerItem],
) -> Vec<ExtensionSlashCommand> {
    let core_tokens: std::collections::BTreeSet<String> = command_table()
        .iter()
        .map(|spec| spec.token.trim_start_matches('/').to_lowercase())
        .collect();
    let mut claimed = core_tokens;
    let mut out = Vec::new();
    for item in items {
        if item.id == "code-swarm" {
            // TUI-side surface (picker + orchestration) keyed to this
            // extension; dispatched by the explicit "/code-swarm" arm, never
            // by the generic ExtensionRun fallback. v1 special case — see
            // docs/reviews/extension-ux-code-swarm-2026-07-09.md.
            claimed.insert("code-swarm".to_owned());
            out.push(ExtensionSlashCommand {
                token: "/code-swarm".to_owned(),
                summary: format!("{} · reviewer swarm", item.id),
                extension_id: item.id.clone(),
                command: "swarm".to_owned(),
                enabled: item.enabled,
            });
        }
        for command in &item.commands {
            let short = command.to_lowercase();
            let (token, summary) = if claimed.contains(&short) {
                (
                    format!("/{}.{}", item.id, command),
                    format!("{} · {}", item.id, command),
                )
            } else {
                claimed.insert(short);
                (format!("/{command}"), format!("{} · {}", item.id, command))
            };
            out.push(ExtensionSlashCommand {
                token,
                summary,
                extension_id: item.id.clone(),
                command: command.clone(),
                enabled: item.enabled,
            });
        }
    }
    out
}

pub fn filter_token(input: &str) -> &str {
    input.split_whitespace().next().unwrap_or(input)
}

fn filter_needle(input: &str) -> String {
    filter_token(input)
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_lowercase()
}

pub fn parse_command(input: &str) -> Result<ParsedCommand<'_>, String> {
    let input = input.trim_start();
    if !input.starts_with('/') {
        return Err("slash command must start with /".to_owned());
    }
    let token_end = input.find(char::is_whitespace).unwrap_or(input.len());
    let token = normalize_command_token(&input[..token_end]);
    let arg = input[token_end..].trim_start();
    Ok(ParsedCommand {
        token,
        arg: (!arg.is_empty()).then_some(arg),
    })
}

pub fn dispatch_command(input: &str, context: &CommandContext) -> CommandEffect {
    match parse_command(input) {
        Ok(parsed) => dispatch_parsed(parsed, context),
        Err(message) => CommandEffect::Message(message),
    }
}

pub fn permission_choices() -> Vec<PermissionChoice> {
    permission_choices_with_grants(&[])
}

/// Mode choices plus revoke rows for active session/project grants.
pub fn permission_choices_with_grants(
    grants: &[(GrantSource, ActiveGrant)],
) -> Vec<PermissionChoice> {
    let mut choices = grants
        .iter()
        .map(|(source, grant)| {
            let pattern = grant.pattern.as_str();
            let pattern_label = if pattern.is_empty() {
                "all".to_owned()
            } else {
                format!("{pattern}*")
            };
            PermissionChoice::Revoke {
                capability: grant.capability,
                pattern: pattern.to_owned(),
                source: *source,
                label: format!(
                    "Revoke {} {} ({})",
                    source.as_str(),
                    grant.capability.as_str(),
                    pattern_label
                ),
            }
        })
        .collect::<Vec<_>>();
    choices.extend(
        [
            Capability::FsRead,
            Capability::FsWrite,
            Capability::ShellExec,
        ]
        .into_iter()
        .flat_map(permission_modes),
    );
    choices
}

pub fn help_text() -> String {
    command_table()
        .iter()
        .map(help_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn dispatch_parsed(parsed: ParsedCommand<'_>, context: &CommandContext) -> CommandEffect {
    match parsed.token {
        "/new" => CommandEffect::Action(CommandAction::NewSession),
        "/model" => model_effect(parsed.arg, context),
        "/effort" => effort_effect(parsed.arg, context),
        "/theme" => theme_effect(parsed.arg, context),
        "/compact" => CommandEffect::Action(CommandAction::CompactSession),
        "/compaction" => CommandEffect::Action(CommandAction::ShowCompaction),
        "/export" => CommandEffect::Action(CommandAction::ExportSession {
            path: parsed.arg.map(str::to_owned),
        }),
        "/extension" => extension_effect(parsed.arg, context),
        "/companion" => companion_effect(parsed.arg),
        "/status" => CommandEffect::Action(CommandAction::ShowStatus),
        "/hotkeys" => CommandEffect::Action(CommandAction::ShowHelp {
            text: hotkeys_text(),
        }),
        "/login" => CommandEffect::Action(CommandAction::Login {
            provider: parsed.arg.unwrap_or("chatgpt").to_owned(),
        }),
        "/logout" => CommandEffect::Action(CommandAction::Logout {
            provider: parsed.arg.unwrap_or("chatgpt").to_owned(),
        }),
        "/name" => required_arg(parsed.arg, "usage: /name <name>", |name| {
            CommandAction::NameSession {
                name: name.to_owned(),
            }
        }),
        "/permissions" => CommandEffect::Action(CommandAction::OpenPermissions),
        "/resume" => CommandEffect::OpenPicker(PickerSpec::Resume(context.resume_items.clone())),
        "/rollback" => rollback_effect(context),
        "/help" => CommandEffect::Action(CommandAction::ShowHelp { text: help_text() }),
        "/quit" => CommandEffect::Action(CommandAction::Quit),
        "/copy" => CommandEffect::Action(CommandAction::CopyLastAssistantResponse),
        "/timestamps" => CommandEffect::Action(CommandAction::ToggleTimestamps),
        "/diff" => CommandEffect::Action(CommandAction::ShowDiff),
        "/usage" => CommandEffect::Action(CommandAction::ShowUsage),
        "/dag" => CommandEffect::Action(CommandAction::DagExport),
        "/code-swarm" => code_swarm_effect(parsed.arg, context),
        token => extension_slash_or_unknown(token, parsed.arg, context),
    }
}

/// `/code-swarm` — extension-provided surface (teaches when disabled).
/// No arg: open the reviewer-model checklist picker (selection IS the count).
/// `review [--personas a,b,c] [--prompt <text…>]`: run the swarm.
fn code_swarm_effect(arg: Option<&str>, context: &CommandContext) -> CommandEffect {
    const USAGE: &str = "usage: /code-swarm  or  /code-swarm review [--personas correctness,safety,tests] [--prompt <focus…>]";
    let enabled = context
        .extension_items
        .iter()
        .find(|item| item.id == "code-swarm")
        .is_some_and(|item| item.enabled);
    if !enabled {
        return CommandEffect::Message(disabled_extension_teach("/code-swarm", "code-swarm"));
    }
    let Some(arg) = arg.map(str::trim).filter(|arg| !arg.is_empty()) else {
        return CommandEffect::OpenPicker(PickerSpec::CodeSwarmModels {
            choices: context.model_choices.clone(),
            selected: context.code_swarm_models.clone(),
        });
    };
    let Some(rest) = arg.strip_prefix("review") else {
        return CommandEffect::Message(USAGE.to_owned());
    };
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return CommandEffect::Message(USAGE.to_owned());
    }
    match parse_code_swarm_review_args(rest.trim_start()) {
        Ok((prompt, personas)) => {
            CommandEffect::Action(CommandAction::CodeSwarmReview { prompt, personas })
        }
        Err(message) => CommandEffect::Message(format!("{message}\n{USAGE}")),
    }
}

/// `--personas a,b,c` and `--prompt <everything after it>`. Prompt must come
/// last so it can contain spaces without quoting.
#[allow(clippy::type_complexity)]
fn parse_code_swarm_review_args(
    rest: &str,
) -> Result<(Option<String>, Option<Vec<String>>), String> {
    let mut prompt = None;
    let mut personas = None;
    let mut remaining = rest.trim();
    while !remaining.is_empty() {
        if let Some(value) = remaining.strip_prefix("--prompt") {
            let value = value.trim();
            if value.is_empty() {
                return Err("--prompt requires text".to_owned());
            }
            prompt = Some(value.to_owned());
            remaining = "";
        } else if let Some(value) = remaining.strip_prefix("--personas") {
            let value = value.trim_start();
            let (list, rest) = match value.split_once(char::is_whitespace) {
                Some((list, rest)) => (list, rest.trim_start()),
                None => (value, ""),
            };
            if list.is_empty() {
                return Err("--personas requires a comma-separated list".to_owned());
            }
            personas = Some(list.split(',').map(str::trim).map(str::to_owned).collect());
            remaining = rest;
        } else {
            return Err(format!("unknown argument: {remaining}"));
        }
    }
    Ok((prompt, personas))
}

fn extension_slash_or_unknown(
    token: &str,
    arg: Option<&str>,
    context: &CommandContext,
) -> CommandEffect {
    if let Some(cmd) = context
        .extension_slash_commands
        .iter()
        .find(|cmd| cmd.token == token)
    {
        if !cmd.enabled {
            return CommandEffect::Message(disabled_extension_teach(&cmd.token, &cmd.extension_id));
        }
        // Arguments are never dropped: JSON parses here, `--flag` text is
        // parsed against the ArgSpec at resolve time, anything else is a
        // usage error.
        let (input, raw_args) = match arg.map(str::trim).filter(|arg| !arg.is_empty()) {
            None => (serde_json::Value::Object(serde_json::Map::new()), None),
            Some(json) if json.starts_with('{') => match serde_json::from_str(json) {
                Ok(value) => (value, None),
                Err(error) => {
                    return CommandEffect::Message(format!("{token} input must be JSON: {error}"));
                }
            },
            Some(flags) if flags.starts_with("--") => (
                serde_json::Value::Object(serde_json::Map::new()),
                Some(flags.to_owned()),
            ),
            Some(other) => {
                return CommandEffect::Message(format!(
                    "usage: {token} [--flag value…]  or  {token} {{json}}  — got `{other}`"
                ));
            }
        };
        return CommandEffect::Action(CommandAction::ExtensionRun {
            id: cmd.extension_id.clone(),
            command: cmd.command.clone(),
            input,
            raw_args,
        });
    }
    CommandEffect::Message(format!("unknown command: {token}"))
}

/// Faint teach line when an extension-backed command is invoked while disabled.
pub fn disabled_extension_teach(token: &str, extension_id: &str) -> String {
    format!("{token} — provided by {extension_id} (disabled) · /extension to enable")
}

fn rollback_effect(context: &CommandContext) -> CommandEffect {
    if context.checkpoint_items.is_empty() {
        return CommandEffect::Message(
            "no workspace checkpoints in this session (edit a file first)".to_owned(),
        );
    }
    CommandEffect::OpenPicker(PickerSpec::Rollback(context.checkpoint_items.clone()))
}

fn companion_effect(arg: Option<&str>) -> CommandEffect {
    let Some(arg) = arg else {
        return CommandEffect::Message("usage: /companion run <json-agent-task>".to_owned());
    };
    let Some(rest) = arg.strip_prefix("run") else {
        return CommandEffect::Message("usage: /companion run <json-agent-task>".to_owned());
    };
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return CommandEffect::Message("usage: /companion run <json-agent-task>".to_owned());
    }
    let json = rest.trim_start();
    if json.is_empty() {
        return CommandEffect::Message("usage: /companion run <json-agent-task>".to_owned());
    }
    match serde_json::from_str(json) {
        Ok(input) => CommandEffect::Action(CommandAction::CompanionRun { input }),
        Err(error) => CommandEffect::Message(format!("companion input must be JSON: {error}")),
    }
}

fn extension_effect(arg: Option<&str>, context: &CommandContext) -> CommandEffect {
    let Some(arg) = arg else {
        return CommandEffect::Action(CommandAction::OpenExtensionManager);
    };
    let Some(rest) = arg.strip_prefix("run") else {
        return CommandEffect::Message(
            "usage: /extension  or  /extension run <ext>.<cmd> [json-input]".to_owned(),
        );
    };
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return CommandEffect::Message(
            "usage: /extension  or  /extension run <ext>.<cmd> [json-input]".to_owned(),
        );
    }
    let mut parts = rest.trim_start().splitn(2, char::is_whitespace);
    let Some(reference) = parts.next().filter(|value| !value.is_empty()) else {
        return CommandEffect::Message(
            "usage: /extension  or  /extension run <ext>.<cmd> [json-input]".to_owned(),
        );
    };
    let Some((id, command)) = parse_extension_reference(reference) else {
        return CommandEffect::Message(
            "usage: /extension  or  /extension run <ext>.<cmd> [json-input]".to_owned(),
        );
    };
    if let Some(item) = context.extension_items.iter().find(|item| item.id == id) {
        if !item.enabled {
            return CommandEffect::Message(disabled_extension_teach(
                &format!("/{id}.{command}"),
                &id,
            ));
        }
    }
    let (input, raw_args) = match parts
        .next()
        .map(str::trim_start)
        .filter(|value| !value.is_empty())
    {
        Some(json) if json.starts_with('{') => match serde_json::from_str(json) {
            Ok(value) => (value, None),
            Err(error) => {
                return CommandEffect::Message(format!("extension input must be JSON: {error}"));
            }
        },
        Some(flags) if flags.starts_with("--") => (
            serde_json::Value::Object(serde_json::Map::new()),
            Some(flags.to_owned()),
        ),
        Some(other) => {
            return CommandEffect::Message(format!(
                "usage: /extension run <ext>.<cmd> [{{json}} | --flag value…]  — got `{other}`"
            ));
        }
        None => (serde_json::Value::Object(serde_json::Map::new()), None),
    };
    CommandEffect::Action(CommandAction::ExtensionRun {
        id,
        command,
        input,
        raw_args,
    })
}

fn parse_extension_reference(reference: &str) -> Option<(String, String)> {
    let (id, command) = reference.split_once('.')?;
    if id.is_empty() || command.is_empty() || command.contains('.') {
        return None;
    }
    Some((id.to_owned(), command.to_owned()))
}

fn model_effect(arg: Option<&str>, context: &CommandContext) -> CommandEffect {
    let Some(target) = arg else {
        return CommandEffect::OpenPicker(PickerSpec::Model(context.model_choices.clone()));
    };
    match parse_model_target(target) {
        Some((provider, model)) => {
            CommandEffect::Action(CommandAction::SwitchModel { provider, model })
        }
        None => CommandEffect::Message("usage: /model <provider::model>".to_owned()),
    }
}

fn effort_effect(arg: Option<&str>, context: &CommandContext) -> CommandEffect {
    let Some(level) = arg else {
        return CommandEffect::OpenPicker(PickerSpec::Effort(context.effort_choices.clone()));
    };
    match ReasoningEffort::parse(level) {
        Some(effort) => CommandEffect::Action(CommandAction::SetReasoningEffort { effort }),
        None => {
            CommandEffect::Message("usage: /effort <xsmall|small|medium|large|xlarge>".to_owned())
        }
    }
}

fn theme_effect(arg: Option<&str>, context: &CommandContext) -> CommandEffect {
    let Some(theme) = arg else {
        return CommandEffect::OpenPicker(PickerSpec::Theme(context.theme_choices.clone()));
    };
    match ThemeChoice::parse(theme) {
        Some(choice) => CommandEffect::Action(CommandAction::SetTheme { choice }),
        None => CommandEffect::Message(format!(
            "usage: /theme <{}>",
            ThemeChoice::format_canonical_ids("|")
        )),
    }
}

pub fn theme_choices(current: ThemeChoice) -> Vec<ThemeChoiceItem> {
    ThemeChoice::all()
        .iter()
        .map(|profile| ThemeChoiceItem {
            choice: profile.choice,
            label: profile.label.to_owned(),
            current: profile.choice == current,
        })
        .collect()
}

fn required_arg(
    arg: Option<&str>,
    usage: &str,
    action: impl FnOnce(&str) -> CommandAction,
) -> CommandEffect {
    match arg {
        Some(value) => CommandEffect::Action(action(value)),
        None => CommandEffect::Message(usage.to_owned()),
    }
}

fn parse_model_target(target: &str) -> Option<(String, String)> {
    let (provider, model) = target.split_once("::").or_else(|| target.split_once('/'))?;
    let provider = provider.trim();
    let model = model.trim();
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    Some((provider.to_ascii_lowercase(), model.to_owned()))
}

fn permission_modes(capability: Capability) -> Vec<PermissionChoice> {
    [
        ApprovalMode::Ask,
        ApprovalMode::SessionAllow,
        ApprovalMode::AlwaysDeny,
    ]
    .into_iter()
    .map(|mode| PermissionChoice::SetMode {
        capability,
        mode,
        label: format!("{} {}", capability_label(capability), mode_label(mode)),
    })
    .collect()
}

fn command_matches(spec: &CommandSpec, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let token = spec.token.trim_start_matches('/').to_lowercase();
    token.starts_with(needle) || token.contains(needle)
}

fn normalize_command_token(token: &str) -> &str {
    let trimmed = token.trim_end_matches('/');
    if trimmed.is_empty() {
        "/"
    } else {
        trimmed
    }
}

fn help_line(spec: &CommandSpec) -> String {
    let args = command_args(spec);
    let args = if args.is_empty() {
        String::new()
    } else {
        format!(" {args}")
    };
    format!("{}{} - {}", spec.token, args, spec.summary)
}

fn command_args(spec: &CommandSpec) -> String {
    if spec.token == "/theme" {
        return format!("[{}]", ThemeChoice::format_canonical_ids("|"));
    }
    spec.args.to_owned()
}

fn hotkeys_text() -> String {
    [
        "Enter - send message",
        "Shift+Enter - insert newline",
        "Esc - close palette, search, or interrupt active turn",
        "Ctrl+O - expand/collapse folded blocks",
        "Ctrl+F - search transcript",
        "@ - mention a workspace file",
        "Ctrl+C twice - quit",
        "Mouse wheel/PageUp/PageDown - scroll transcript",
    ]
    .join("\n")
}

fn capability_label(capability: Capability) -> &'static str {
    capability.as_str()
}

fn mode_label(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Ask => "ask",
        ApprovalMode::SessionAllow => "session-allow",
        ApprovalMode::AlwaysDeny => "always-deny",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_command_parsing_routes_baseline_actions() {
        let context = CommandContext::default();

        assert_eq!(
            dispatch_command("/model openrouter::glm-5.2", &context),
            CommandEffect::Action(CommandAction::SwitchModel {
                provider: "openrouter".to_owned(),
                model: "glm-5.2".to_owned(),
            })
        );
        assert_eq!(
            dispatch_command("/model openrouter/openai/gpt-4.1-mini", &context),
            CommandEffect::Action(CommandAction::SwitchModel {
                provider: "openrouter".to_owned(),
                model: "openai/gpt-4.1-mini".to_owned(),
            })
        );
        assert_eq!(
            dispatch_command("/model OpenRouter::openai/gpt-4.1-mini  ", &context),
            CommandEffect::Action(CommandAction::SwitchModel {
                provider: "openrouter".to_owned(),
                model: "openai/gpt-4.1-mini".to_owned(),
            })
        );
        assert_eq!(
            dispatch_command("/effort xlarge", &context),
            CommandEffect::Action(CommandAction::SetReasoningEffort {
                effort: ReasoningEffort::XLarge,
            })
        );
        assert_eq!(
            dispatch_command("/name research branch", &context),
            CommandEffect::Action(CommandAction::NameSession {
                name: "research branch".to_owned(),
            })
        );
        assert_eq!(
            dispatch_command("/copy", &context),
            CommandEffect::Action(CommandAction::CopyLastAssistantResponse)
        );
        assert_eq!(
            dispatch_command("/timestamps", &context),
            CommandEffect::Action(CommandAction::ToggleTimestamps)
        );
        assert_eq!(
            dispatch_command("/quit", &context),
            CommandEffect::Action(CommandAction::Quit)
        );
        assert_eq!(
            dispatch_command("/new", &context),
            CommandEffect::Action(CommandAction::NewSession)
        );
        assert_eq!(
            dispatch_command("/status", &context),
            CommandEffect::Action(CommandAction::ShowStatus)
        );
        assert_eq!(
            dispatch_command("/compact", &context),
            CommandEffect::Action(CommandAction::CompactSession)
        );
        assert_eq!(
            dispatch_command("/export /tmp/euler.json", &context),
            CommandEffect::Action(CommandAction::ExportSession {
                path: Some("/tmp/euler.json".to_owned()),
            })
        );
        assert_eq!(
            dispatch_command(
                "/extension run session-export.session-export {\"limit\":1}",
                &context
            ),
            CommandEffect::Action(CommandAction::ExtensionRun {
                id: "session-export".to_owned(),
                command: "session-export".to_owned(),
                input: serde_json::json!({"limit": 1}),
                raw_args: None,
            })
        );
        assert_eq!(
            dispatch_command("/extension run causal-dag.catch-up", &context),
            CommandEffect::Action(CommandAction::ExtensionRun {
                id: "causal-dag".to_owned(),
                command: "catch-up".to_owned(),
                input: serde_json::json!({}),
                raw_args: None,
            })
        );
        assert_eq!(
            dispatch_command(
                "/companion run {\"task\":\"review\",\"persona\":\"worker\"}",
                &context
            ),
            CommandEffect::Action(CommandAction::CompanionRun {
                input: serde_json::json!({"task": "review", "persona": "worker"}),
            })
        );
        assert_eq!(
            dispatch_command("/theme light", &context),
            CommandEffect::Action(CommandAction::SetTheme {
                choice: ThemeChoice::GruvboxLight,
            })
        );
        assert_eq!(
            dispatch_command("/theme dark", &context),
            CommandEffect::Action(CommandAction::SetTheme {
                choice: ThemeChoice::GruvboxDark,
            })
        );
        assert_eq!(
            dispatch_command("/theme gruvbox_dark", &context),
            CommandEffect::Action(CommandAction::SetTheme {
                choice: ThemeChoice::GruvboxDark,
            })
        );
        assert_eq!(
            dispatch_command("/login", &context),
            CommandEffect::Action(CommandAction::Login {
                provider: "chatgpt".to_owned(),
            })
        );
    }

    #[test]
    fn direct_dispatch_uses_visible_token_for_trailing_slash_noise() {
        let context = CommandContext::default();

        assert_eq!(
            dispatch_command("/effort// large", &context),
            CommandEffect::Action(CommandAction::SetReasoningEffort {
                effort: ReasoningEffort::Large,
            })
        );
    }

    #[test]
    fn model_without_arg_opens_caller_supplied_picker() {
        let context = CommandContext {
            model_choices: vec![ModelChoice::new("fixture", "echo")],
            ..CommandContext::default()
        };

        assert_eq!(
            dispatch_command("/model", &context),
            CommandEffect::OpenPicker(PickerSpec::Model(vec![ModelChoice::new("fixture", "echo")]))
        );
    }

    #[test]
    fn rollback_without_checkpoints_messages_and_with_items_opens_picker() {
        assert_eq!(
            dispatch_command("/rollback", &CommandContext::default()),
            CommandEffect::Message(
                "no workspace checkpoints in this session (edit a file first)".to_owned()
            )
        );
        let item = CheckpointItem::new(
            "01ABCDEFGHJKLMNPQRSTUVWXYZ",
            "modify",
            "src/lib.rs",
            "2026-07-09T12:34:56.000Z",
        );
        let context = CommandContext {
            checkpoint_items: vec![item.clone()],
            ..CommandContext::default()
        };
        assert_eq!(
            dispatch_command("/rollback", &context),
            CommandEffect::OpenPicker(PickerSpec::Rollback(vec![item]))
        );
    }

    #[test]
    fn model_choice_label_includes_known_metadata_without_changing_value_fields() {
        let choice = ModelChoice::with_metadata("openai", "gpt-5.5", Some(1_050_000), Some(true));

        assert_eq!(choice.provider, "openai");
        assert_eq!(choice.model, "gpt-5.5");
        assert_eq!(choice.label, "openai::gpt-5.5 — 1.05M ctx, reasoning");

        let unknown = ModelChoice::with_metadata("custom", "future", None, None);
        assert_eq!(unknown.label, "custom::future");
    }

    #[test]
    fn context_token_labels_keep_non_whole_millions_honest() {
        assert_eq!(format_context_tokens(200_000), "200K");
        assert_eq!(format_context_tokens(400_000), "400K");
        assert_eq!(format_context_tokens(1_000_000), "1M");
        assert_eq!(format_context_tokens(1_050_000), "1.05M");
    }

    #[test]
    fn command_filter_uses_first_token_and_prefix_or_substring() {
        let model = filter_commands("/mo ignored");
        let substring = filter_commands("/del");

        assert_eq!(
            model.iter().map(|spec| spec.token).collect::<Vec<_>>(),
            vec!["/model"]
        );
        assert_eq!(
            substring.iter().map(|spec| spec.token).collect::<Vec<_>>(),
            vec!["/model"]
        );
    }

    #[test]
    fn help_text_uses_catalog_theme_usage() {
        assert!(help_text().contains(&format!(
            "/theme [{}] - switch theme",
            ThemeChoice::format_canonical_ids("|")
        )));
        assert!(command_table()
            .iter()
            .any(|spec| spec.token == "/theme" && spec.args.is_empty()));
        assert!(command_table().iter().any(|spec| spec.token == "/diff"));
        assert!(command_table().iter().any(|spec| spec.token == "/usage"));
        assert!(command_table().iter().any(|spec| spec.token == "/dag"));
        assert!(command_table().iter().any(|spec| spec.token == "/rollback"));
        assert!(command_table()
            .iter()
            .any(|spec| spec.token == "/timestamps"));
    }

    #[test]
    fn extension_slash_collisions_prefer_core_token_and_qualified_extension() {
        let items = vec![ExtensionManagerItem {
            id: "causal-dag".to_owned(),
            display_name: "Causal DAG".to_owned(),
            enabled: true,
            bundled: true,
            materialization: None,
            version: "0.1.0".to_owned(),
            commands: vec!["export".to_owned(), "catch-up".to_owned()],
            capabilities: vec![],
            audit_status: None,
        }];
        let cmds = build_extension_slash_commands(&items);
        assert!(cmds.iter().any(|c| c.token == "/causal-dag.export"));
        assert!(cmds.iter().any(|c| c.token == "/catch-up"));
        assert!(!cmds.iter().any(|c| c.token == "/export"));
    }

    fn code_swarm_context(enabled: bool) -> CommandContext {
        CommandContext {
            model_choices: vec![
                ModelChoice::new("openrouter", "z-ai/glm-5.2"),
                ModelChoice::new("anthropic", "claude-opus-5"),
                ModelChoice::new("openai", "gpt-5.5"),
                ModelChoice::new("mistral", "large-3"),
            ],
            extension_items: vec![ExtensionManagerItem {
                id: "code-swarm".to_owned(),
                display_name: "CodeSwarm Review".to_owned(),
                enabled,
                bundled: true,
                materialization: None,
                version: "0.1.0".to_owned(),
                commands: vec!["review-brief".to_owned(), "review-report".to_owned()],
                capabilities: vec![],
                audit_status: None,
            }],
            ..CommandContext::default()
        }
    }

    #[test]
    fn code_swarm_no_arg_opens_model_checklist_picker() {
        let context = code_swarm_context(true);
        match dispatch_command("/code-swarm", &context) {
            CommandEffect::OpenPicker(PickerSpec::CodeSwarmModels { choices, selected }) => {
                assert_eq!(choices.len(), 4);
                assert!(selected.is_empty());
            }
            other => panic!("expected picker, got {other:?}"),
        }
    }

    #[test]
    fn code_swarm_review_parses_personas_and_trailing_prompt() {
        let context = code_swarm_context(true);
        assert_eq!(
            dispatch_command(
                "/code-swarm review --personas tests,safety --prompt focus on the retry logic",
                &context
            ),
            CommandEffect::Action(CommandAction::CodeSwarmReview {
                prompt: Some("focus on the retry logic".to_owned()),
                personas: Some(vec!["tests".to_owned(), "safety".to_owned()]),
            })
        );
        assert_eq!(
            dispatch_command("/code-swarm review", &context),
            CommandEffect::Action(CommandAction::CodeSwarmReview {
                prompt: None,
                personas: None,
            })
        );
        assert!(matches!(
            dispatch_command("/code-swarm review --bogus", &context),
            CommandEffect::Message(message) if message.contains("usage:")
        ));
        assert!(matches!(
            dispatch_command("/code-swarm bogus", &context),
            CommandEffect::Message(message) if message.contains("usage:")
        ));
    }

    #[test]
    fn code_swarm_disabled_teaches_and_palette_entry_registers() {
        let context = code_swarm_context(false);
        assert_eq!(
            dispatch_command("/code-swarm", &context),
            CommandEffect::Message(disabled_extension_teach("/code-swarm", "code-swarm"))
        );

        let cmds = build_extension_slash_commands(&context.extension_items);
        let entry = cmds
            .iter()
            .find(|cmd| cmd.token == "/code-swarm")
            .expect("code-swarm palette entry");
        assert_eq!(entry.extension_id, "code-swarm");
        assert!(entry.summary.contains("reviewer swarm"));
        assert!(!entry.enabled);
        // The command tokens still register alongside the surface token.
        assert!(cmds.iter().any(|cmd| cmd.token == "/review-brief"));
    }

    #[test]
    fn extension_slash_arguments_parse_or_reject_never_drop() {
        let context = CommandContext {
            extension_slash_commands: vec![ExtensionSlashCommand {
                token: "/review-brief".to_owned(),
                summary: "code-swarm · review-brief".to_owned(),
                extension_id: "code-swarm".to_owned(),
                command: "review-brief".to_owned(),
                enabled: true,
            }],
            ..CommandContext::default()
        };

        // JSON argument parses into the input.
        assert_eq!(
            dispatch_command("/review-brief {\"reviewers\":[\"tests\"]}", &context),
            CommandEffect::Action(CommandAction::ExtensionRun {
                id: "code-swarm".to_owned(),
                command: "review-brief".to_owned(),
                input: serde_json::json!({"reviewers": ["tests"]}),
                raw_args: None,
            })
        );
        // Flag arguments travel to resolve-time ArgSpec parsing.
        assert_eq!(
            dispatch_command("/review-brief --reviewer tests --model a::b", &context),
            CommandEffect::Action(CommandAction::ExtensionRun {
                id: "code-swarm".to_owned(),
                command: "review-brief".to_owned(),
                input: serde_json::json!({}),
                raw_args: Some("--reviewer tests --model a::b".to_owned()),
            })
        );
        // Invalid JSON is an error, not a silent default run.
        assert!(matches!(
            dispatch_command("/review-brief {broken", &context),
            CommandEffect::Message(message) if message.contains("must be JSON")
        ));
        // Free text is a usage error, not a silent default run.
        assert!(matches!(
            dispatch_command("/review-brief tests please", &context),
            CommandEffect::Message(message) if message.contains("usage:")
        ));
        // No argument still dispatches with empty input.
        assert_eq!(
            dispatch_command("/review-brief", &context),
            CommandEffect::Action(CommandAction::ExtensionRun {
                id: "code-swarm".to_owned(),
                command: "review-brief".to_owned(),
                input: serde_json::json!({}),
                raw_args: None,
            })
        );
    }

    #[test]
    fn disabled_extension_slash_teaches_instead_of_unknown() {
        let context = CommandContext {
            extension_slash_commands: vec![ExtensionSlashCommand {
                token: "/catch-up".to_owned(),
                summary: "causal-dag · catch-up".to_owned(),
                extension_id: "causal-dag".to_owned(),
                command: "catch-up".to_owned(),
                enabled: false,
            }],
            ..CommandContext::default()
        };
        assert_eq!(
            dispatch_command("/catch-up", &context),
            CommandEffect::Message(disabled_extension_teach("/catch-up", "causal-dag"))
        );
    }

    #[test]
    fn invalid_or_incomplete_commands_return_messages() {
        let context = CommandContext::default();

        assert_eq!(
            dispatch_command("model fixture/echo", &context),
            CommandEffect::Message("slash command must start with /".to_owned())
        );
        assert_eq!(
            dispatch_command("/model missing-separator", &context),
            CommandEffect::Message("usage: /model <provider::model>".to_owned())
        );
        assert_eq!(
            dispatch_command("/effort", &context),
            CommandEffect::OpenPicker(PickerSpec::Effort(Vec::new()))
        );
        assert_eq!(
            dispatch_command("/effort extra-high", &context),
            CommandEffect::Message("usage: /effort <xsmall|small|medium|large|xlarge>".to_owned())
        );
        assert_eq!(
            dispatch_command("/theme gruvbox", &context),
            CommandEffect::Message(format!(
                "usage: /theme <{}>",
                ThemeChoice::format_canonical_ids("|")
            ))
        );
        assert_eq!(
            dispatch_command("/extension", &context),
            CommandEffect::Action(CommandAction::OpenExtensionManager)
        );
        assert_eq!(
            dispatch_command("/extension run causal-dag", &context),
            CommandEffect::Message(
                "usage: /extension  or  /extension run <ext>.<cmd> [json-input]".to_owned()
            )
        );
        assert_eq!(
            dispatch_command("/diff", &context),
            CommandEffect::Action(CommandAction::ShowDiff)
        );
        assert_eq!(
            dispatch_command("/usage", &context),
            CommandEffect::Action(CommandAction::ShowUsage)
        );
        assert_eq!(
            dispatch_command("/dag", &context),
            CommandEffect::Action(CommandAction::DagExport)
        );
        assert!(matches!(
            dispatch_command("/extension run causal-dag.catch-up {", &context),
            CommandEffect::Message(message) if message.starts_with("extension input must be JSON:")
        ));
        assert_eq!(
            dispatch_command("/unknown", &context),
            CommandEffect::Message("unknown command: /unknown".to_owned())
        );
    }
}
