use super::theme::ThemeChoice;
use euler_core::{ActiveGrant, ApprovalMode, GrantSource, ReasoningEffort};
use euler_sdk::Capability;

mod causal_dag;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub token: &'static str,
    pub summary: &'static str,
    pub args: &'static str,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandContext {
    pub model_choices: Vec<ModelChoice>,
    /// Same shape as `model_choices`, filtered to providers whose auth
    /// `ProviderSet::is_authenticated` accepts today. Feeds the
    /// `/code-swarm` reviewer-model picker so it cannot offer a target that
    /// only fails once a spawn slot is already spent (#58).
    pub code_swarm_model_choices: Vec<ModelChoice>,
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
    /// Current causal-DAG counts for its extension-owned picker surface.
    pub causal_dag_stats: Option<CausalDagStats>,
    /// Current session-local compaction controls for `/compaction`.
    pub compaction: CompactionSettings,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactionSettings {
    pub automatic: bool,
    pub stubs: bool,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            automatic: true,
            stubs: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CausalDagStats {
    pub session_id: String,
    pub node_count: usize,
    pub cross_arc_count: usize,
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
    pub commands: Vec<ExtensionCommandItem>,
    pub capabilities: Vec<String>,
    pub audit_status: Option<String>,
}

/// One command an extension registers, and whether a user may drive it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionCommandItem {
    pub name: String,
    pub invocation: euler_sdk::Invocation,
}

impl ExtensionCommandItem {
    /// Fixture helper: production builds these from real descriptors.
    #[cfg(test)]
    pub fn user(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            invocation: euler_sdk::Invocation::User,
        }
    }
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
            // Agent-only commands are listed, not hidden: they exist, and the
            // reader should know why no slash command matches them.
            let commands = self
                .commands
                .iter()
                .map(|command| {
                    if command.invocation.is_agent_only() {
                        format!("{} (agent-only)", command.name)
                    } else {
                        command.name.clone()
                    }
                })
                .collect::<Vec<_>>();
            lines.push(format!("commands: {}", commands.join(", ")));
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
            ReasoningEffort::XLarge => "xlarge - extra-high reasoning",
            ReasoningEffort::Max => "max - maximum reasoning",
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

/// A session-local operating posture. It only configures permission policy;
/// it never claims to create an operating-system sandbox.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionPosture {
    /// Allow the capabilities that only retrieve local/session information;
    /// deny mutation, execution, network, and credential access.
    ReadOnly,
    /// Require an explicit decision for every capability.
    AskEveryTime,
    /// Allow every capability for this session without a sandbox boundary.
    FullAccess,
}

impl PermissionPosture {
    pub const ALL: [Self; 3] = [Self::ReadOnly, Self::AskEveryTime, Self::FullAccess];

    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "Read only",
            Self::AskEveryTime => "Ask every time",
            Self::FullAccess => "Full access (unsandboxed)",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            Self::ReadOnly => {
                "allow local/session reads; deny writes, commands, agents, network, and secrets"
            }
            Self::AskEveryTime => {
                "clear session approvals; ask before each operation not covered by a durable rule"
            }
            Self::FullAccess => "allow every capability for this session; no OS sandbox is active",
        }
    }

    pub fn mode_for(self, capability: Capability) -> ApprovalMode {
        match self {
            Self::ReadOnly => match capability {
                Capability::FsRead | Capability::ProvenanceRead | Capability::DiagnosticsRead => {
                    ApprovalMode::SessionAllow
                }
                _ => ApprovalMode::AlwaysDeny,
            },
            Self::AskEveryTime => ApprovalMode::Ask,
            Self::FullAccess => ApprovalMode::SessionAllow,
        }
    }
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
    SetCompactionPolicy {
        automatic: bool,
        stubs: bool,
    },
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
    /// Persist the `/code-swarm` reviewer model set (1-5 provider::model)
    /// to the project tier, or the user-global tier with `--user`.
    CodeSwarmSaveModels {
        models: Vec<String>,
        user_tier: bool,
    },
    /// Remove one tier's persisted `/code-swarm` reviewer config.
    CodeSwarmClear {
        user_tier: bool,
    },
    ShowStatus,
    /// Remove a credential from every provenance surface (issue #100). Bare
    /// (`value: None`) scrubs the values detected in tool-call arguments this
    /// session; an explicit value scrubs exactly that string.
    Scrub {
        value: Option<String>,
    },
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
    SetPermissionPosture {
        posture: PermissionPosture,
    },
    /// Kept as a visible picker row so users understand the intended future
    /// posture without mistaking an approval policy for a sandbox.
    PermissionSandboxUnavailable,
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
    /// Drill from the causal-DAG action picker into its export formats.
    OpenCausalDagExport {
        stats: CausalDagStats,
    },
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
    /// Muted, non-error informational line (review v2 §14.4) — e.g. the
    /// disabled-extension teach message. Never styled red, never prefixed.
    Notice(String),
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
    /// `user_tier` routes the save to the user-global store.
    CodeSwarmModels {
        choices: Vec<ModelChoice>,
        selected: Vec<String>,
        user_tier: bool,
    },
    CausalDagActions(CausalDagStats),
    CausalDagFormats(CausalDagStats),
    Compaction(CompactionSettings),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermissionChoice {
    Posture {
        posture: PermissionPosture,
        label: String,
        detail: String,
    },
    Unavailable {
        label: String,
        detail: String,
    },
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
        summary: "configure automatic compaction and stubs",
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
        token: "/scrub",
        summary: "remove a detected credential from provenance",
        args: "[value]",
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
        summary: "session permission postures, advanced modes, and active grants",
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

#[cfg(test)]
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
        if item.id == "causal-dag" {
            claimed.insert("causal-dag".to_owned());
            out.push(ExtensionSlashCommand {
                token: "/causal-dag".to_owned(),
                summary: "causal-dag · view, export, or refresh".to_owned(),
                extension_id: item.id.clone(),
                command: "surface".to_owned(),
                enabled: item.enabled,
            });
            continue;
        }
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
                command: "review".to_owned(),
                enabled: item.enabled,
            });
        }
        for command in &item.commands {
            // Agent-only commands mint no slash token: they are the agent's
            // to call, not the user's to drive.
            if command.invocation.is_agent_only() {
                continue;
            }
            let name = &command.name;
            let short = name.to_lowercase();
            let (token, summary) = if claimed.contains(&short) {
                (
                    format!("/{}.{}", item.id, name),
                    format!("{} · {}", item.id, name),
                )
            } else {
                claimed.insert(short);
                (format!("/{name}"), format!("{} · {}", item.id, name))
            };
            out.push(ExtensionSlashCommand {
                token,
                summary,
                extension_id: item.id.clone(),
                command: name.clone(),
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

#[cfg(test)]
pub fn permission_choices() -> Vec<PermissionChoice> {
    permission_choices_with_grants(&[])
}

/// Quick session postures, active grants, then per-capability advanced modes.
pub fn permission_choices_with_grants(
    grants: &[(GrantSource, ActiveGrant)],
) -> Vec<PermissionChoice> {
    let mut choices = PermissionPosture::ALL
        .into_iter()
        .map(|posture| PermissionChoice::Posture {
            posture,
            label: posture.label().to_owned(),
            detail: posture.detail().to_owned(),
        })
        .collect::<Vec<_>>();
    choices.push(PermissionChoice::Unavailable {
        label: "Auto in workspace sandbox (not available)".to_owned(),
        detail: "requires the Linux workspace sandbox; selecting this does not change permissions"
            .to_owned(),
    });
    choices.extend(grants.iter().map(|(source, grant)| {
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
    }));
    choices.extend(Capability::ALL.iter().copied().flat_map(permission_modes));
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
        "/compaction" => CommandEffect::OpenPicker(PickerSpec::Compaction(context.compaction)),
        "/export" => CommandEffect::Action(CommandAction::ExportSession {
            path: parsed.arg.map(str::to_owned),
        }),
        "/extension" => extension_effect(parsed.arg, context),
        "/companion" => companion_effect(parsed.arg),
        "/status" => CommandEffect::Action(CommandAction::ShowStatus),
        "/scrub" => CommandEffect::Action(CommandAction::Scrub {
            value: parsed.arg.map(str::to_owned),
        }),
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
        "/causal-dag" => causal_dag::effect(parsed.arg, context),
        "/code-swarm" => code_swarm_effect(parsed.arg, context),
        token => extension_slash_or_unknown(token, parsed.arg, context),
    }
}

/// `/code-swarm` — set up the reviewer swarm; it does not run one.
///
/// No arg: open the reviewer-model checklist picker (selection IS the
/// count); the save persists to the project tier, or the user tier with
/// `--user`. `clear [--user]` removes one tier's config.
///
/// There is deliberately no run verb here. CodeSwarm is something the agent
/// does when asked ("code swarm this"), through its `code_swarm_review` tool,
/// so this surface configures the reviewers and the agent runs them.
fn code_swarm_effect(arg: Option<&str>, context: &CommandContext) -> CommandEffect {
    const USAGE: &str =
        "usage: /code-swarm [--user]  ·  /code-swarm clear [--user]  —  configures reviewers; \
         to run a review, ask the agent (e.g. \"code swarm this\")";
    let enabled = context
        .extension_items
        .iter()
        .find(|item| item.id == "code-swarm")
        .is_some_and(|item| item.enabled);
    if !enabled {
        return CommandEffect::Notice(disabled_extension_teach("/code-swarm", "code-swarm"));
    }
    let arg = arg.map(str::trim).filter(|arg| !arg.is_empty());
    match arg {
        None => CommandEffect::OpenPicker(PickerSpec::CodeSwarmModels {
            // #58: authenticated providers only — never offer a target that
            // burns a spawn slot to discover it has no credentials.
            choices: context.code_swarm_model_choices.clone(),
            selected: context.code_swarm_models.clone(),
            user_tier: false,
        }),
        Some("--user") => CommandEffect::OpenPicker(PickerSpec::CodeSwarmModels {
            choices: context.code_swarm_model_choices.clone(),
            selected: context.code_swarm_models.clone(),
            user_tier: true,
        }),
        Some("clear") => CommandEffect::Action(CommandAction::CodeSwarmClear { user_tier: false }),
        Some("clear --user") => {
            CommandEffect::Action(CommandAction::CodeSwarmClear { user_tier: true })
        }
        // `review` was a run verb here until CodeSwarm became agent-only.
        // Users still have the muscle memory, so name the replacement rather
        // than emit a bare usage line.
        Some(arg) if arg == "review" || arg.starts_with("review ") => {
            CommandEffect::Message(CODE_SWARM_AGENT_ONLY_TEACH.to_owned())
        }
        Some(_) => CommandEffect::Message(USAGE.to_owned()),
    }
}

/// Shown wherever a user reaches for a CodeSwarm run verb. It names the one
/// way in rather than only refusing.
pub(crate) const CODE_SWARM_AGENT_ONLY_TEACH: &str =
    "code-swarm has no run command: reviews are something the agent runs for you. \
     Ask it in plain language — for example \"code swarm this diff\" or \"code swarm \
     PR 123\" — and it will gather the needed material with ordinary tools, then call \
     its code_swarm_review tool with explicit context. \
     Use /code-swarm to choose which reviewer models it uses.";

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
            return CommandEffect::Notice(disabled_extension_teach(&cmd.token, &cmd.extension_id));
        }
        // Arguments are never dropped: JSON parses here, `--flag` text is
        // parsed against the ArgSpec at resolve time, anything else is a
        // usage error.
        let (input, raw_args) = match extension_argument_values(arg, token) {
            Ok(values) => values,
            Err(message) => return CommandEffect::Message(message),
        };
        return CommandEffect::Action(CommandAction::ExtensionRun {
            id: cmd.extension_id.clone(),
            command: cmd.command.clone(),
            input,
            raw_args,
        });
    }
    // An agent-only command mints no token, so its name lands here. Saying
    // "unknown command" would be a lie: the command exists, it just is not
    // the user's to run.
    if let Some(teach) = agent_only_teach(token, context) {
        return CommandEffect::Message(teach);
    }
    CommandEffect::Message(format!("unknown command: {token}"))
}

/// The teach for a token that names an enabled extension's agent-only command.
fn agent_only_teach(token: &str, context: &CommandContext) -> Option<String> {
    let name = token.trim_start_matches('/');
    let (name, id_hint) = match name.split_once('.') {
        Some((id, command)) => (command, Some(id)),
        None => (name, None),
    };
    let item = context.extension_items.iter().find(|item| {
        item.enabled
            && id_hint.is_none_or(|id| id == item.id)
            && item.commands.iter().any(|command| {
                command.invocation.is_agent_only() && command.name.eq_ignore_ascii_case(name)
            })
    })?;
    if item.id == "code-swarm" {
        return Some(CODE_SWARM_AGENT_ONLY_TEACH.to_owned());
    }
    Some(format!(
        "{token} is not a user command: {} · {name} is run by the agent on your behalf. \
         Ask the agent for it in plain language.",
        item.id
    ))
}

fn extension_argument_values(
    arg: Option<&str>,
    token: &str,
) -> Result<(serde_json::Value, Option<String>), String> {
    match arg.map(str::trim).filter(|arg| !arg.is_empty()) {
        None => Ok((serde_json::Value::Object(serde_json::Map::new()), None)),
        Some(json) if json.starts_with('{') => serde_json::from_str(json)
            .map(|value| (value, None))
            .map_err(|error| format!("{token} input must be JSON: {error}")),
        Some(flags) if flags.starts_with("--") => Ok((
            serde_json::Value::Object(serde_json::Map::new()),
            Some(flags.to_owned()),
        )),
        Some(other) => Err(format!(
            "usage: {token} [--flag value…]  or  {token} {{json}}  — got `{other}`"
        )),
    }
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
            return CommandEffect::Notice(disabled_extension_teach(
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
        None => CommandEffect::Message(
            "usage: /effort <xsmall|small|medium|large|xlarge|max>".to_owned(),
        ),
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
    fn permission_postures_have_explicit_non_sandboxed_mappings() {
        for capability in [
            Capability::FsRead,
            Capability::ProvenanceRead,
            Capability::DiagnosticsRead,
        ] {
            assert_eq!(
                PermissionPosture::ReadOnly.mode_for(capability),
                ApprovalMode::SessionAllow
            );
        }
        for capability in [
            Capability::FsWrite,
            Capability::ShellExec,
            Capability::AgentSpawn,
            Capability::Network,
            Capability::SecretResolve,
        ] {
            assert_eq!(
                PermissionPosture::ReadOnly.mode_for(capability),
                ApprovalMode::AlwaysDeny
            );
            assert_eq!(
                PermissionPosture::AskEveryTime.mode_for(capability),
                ApprovalMode::Ask
            );
            assert_eq!(
                PermissionPosture::FullAccess.mode_for(capability),
                ApprovalMode::SessionAllow
            );
        }
        assert!(PermissionPosture::FullAccess
            .detail()
            .contains("no OS sandbox"));
    }

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
            dispatch_command("/effort max", &context),
            CommandEffect::Action(CommandAction::SetReasoningEffort {
                effort: ReasoningEffort::Max,
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
        let compaction_context = CommandContext {
            compaction: CompactionSettings {
                automatic: true,
                stubs: true,
            },
            ..CommandContext::default()
        };
        assert_eq!(
            dispatch_command("/compaction", &compaction_context),
            CommandEffect::OpenPicker(PickerSpec::Compaction(CompactionSettings {
                automatic: true,
                stubs: true,
            }))
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
        assert!(!command_table().iter().any(|spec| spec.token == "/dag"));
        assert!(command_table().iter().any(|spec| spec.token == "/rollback"));
        assert!(command_table()
            .iter()
            .any(|spec| spec.token == "/timestamps"));
    }

    fn code_swarm_context(enabled: bool) -> CommandContext {
        let model_choices = vec![
            ModelChoice::new("openrouter", "z-ai/glm-5.2"),
            ModelChoice::new("anthropic", "claude-opus-5"),
            ModelChoice::new("openai", "gpt-5.5"),
            ModelChoice::new("mistral", "large-3"),
        ];
        CommandContext {
            // All four are "authenticated" in this fixture; the filtering
            // behavior itself is covered by the dedicated picker-filter test
            // below.
            code_swarm_model_choices: model_choices.clone(),
            model_choices,
            extension_items: vec![ExtensionManagerItem {
                id: "code-swarm".to_owned(),
                display_name: "CodeSwarm Review".to_owned(),
                enabled,
                bundled: true,
                materialization: None,
                version: "0.1.0".to_owned(),
                // Mirrors production: the real ReviewCommand descriptor is
                // AgentOnly, so a fixture claiming otherwise would test a
                // surface that does not exist.
                commands: vec![ExtensionCommandItem {
                    name: "review".to_owned(),
                    invocation: euler_sdk::Invocation::AgentOnly,
                }],
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
            CommandEffect::OpenPicker(PickerSpec::CodeSwarmModels {
                choices,
                selected,
                user_tier: false,
            }) => {
                assert_eq!(choices.len(), 4);
                assert!(selected.is_empty());
            }
            other => panic!("expected picker, got {other:?}"),
        }
    }

    #[test]
    fn code_swarm_user_flag_routes_picker_to_user_tier() {
        let context = code_swarm_context(true);
        match dispatch_command("/code-swarm --user", &context) {
            CommandEffect::OpenPicker(PickerSpec::CodeSwarmModels {
                user_tier: true, ..
            }) => {}
            other => panic!("expected user-tier picker, got {other:?}"),
        }
    }

    #[test]
    fn code_swarm_picker_uses_authenticated_choices_not_all_model_choices() {
        // #58: the picker must not offer a provider that will burn a spawn
        // slot to discover it isn't authenticated. code_swarm_model_choices
        // is the pre-filtered list; the picker must read that field, not the
        // unfiltered model_choices.
        let mut context = code_swarm_context(true);
        context.code_swarm_model_choices = vec![ModelChoice::new("openai", "gpt-5.5")];
        match dispatch_command("/code-swarm", &context) {
            CommandEffect::OpenPicker(PickerSpec::CodeSwarmModels { choices, .. }) => {
                assert_eq!(choices, vec![ModelChoice::new("openai", "gpt-5.5")]);
            }
            other => panic!("expected picker, got {other:?}"),
        }
    }

    #[test]
    fn code_swarm_clear_targets_one_tier() {
        let context = code_swarm_context(true);
        assert_eq!(
            dispatch_command("/code-swarm clear", &context),
            CommandEffect::Action(CommandAction::CodeSwarmClear { user_tier: false })
        );
        assert_eq!(
            dispatch_command("/code-swarm clear --user", &context),
            CommandEffect::Action(CommandAction::CodeSwarmClear { user_tier: true })
        );
    }

    #[test]
    fn code_swarm_has_no_run_verb_and_teaches_the_agent_path() {
        // CodeSwarm is agent-only: /code-swarm configures reviewers, and the
        // agent's code_swarm_review tool is the only way to run one. The old
        // `review` verb has muscle memory behind it, so it must name the
        // replacement rather than emit a bare usage line.
        let context = code_swarm_context(true);
        for input in [
            "/code-swarm review",
            "/code-swarm review --personas tests,safety --prompt focus on the retry logic",
        ] {
            match dispatch_command(input, &context) {
                CommandEffect::Message(message) => {
                    assert!(
                        message.contains("code swarm this") && message.contains("/code-swarm"),
                        "must name the agent path and the config surface: {message}"
                    );
                    assert!(
                        !message.contains("--prompt"),
                        "must not teach a flag that no longer exists: {message}"
                    );
                }
                other => panic!("{input} must not run a review, got {other:?}"),
            }
        }
        assert!(matches!(
            dispatch_command("/code-swarm bogus", &context),
            CommandEffect::Message(message) if message.contains("usage:")
        ));
    }

    #[test]
    fn agent_only_command_mints_no_slash_token_but_is_still_listed() {
        // The command exists; it is simply not the user's to run. Hiding it
        // from the manager would be a different lie than offering to run it.
        let context = code_swarm_context(true);
        assert!(
            !build_extension_slash_commands(&context.extension_items)
                .iter()
                .any(|cmd| cmd.token == "/review"),
            "agent-only review must not mint /review"
        );
        let details = context.extension_items[0].details_text();
        assert!(
            details.contains("review (agent-only)"),
            "manager must still list it, marked: {details}"
        );
    }

    #[test]
    fn agent_only_token_teaches_instead_of_unknown_command() {
        // /review no longer resolves; "unknown command" would be a lie.
        let context = code_swarm_context(true);
        match dispatch_command("/review --prompt x", &context) {
            CommandEffect::Message(message) => {
                assert!(
                    message.contains("code swarm this"),
                    "must name the agent path: {message}"
                );
                assert!(
                    !message.contains("unknown command"),
                    "the command exists: {message}"
                );
            }
            other => panic!("expected teach, got {other:?}"),
        }
    }

    #[test]
    fn code_swarm_disabled_teaches_and_palette_entry_registers() {
        let context = code_swarm_context(false);
        assert_eq!(
            dispatch_command("/code-swarm", &context),
            CommandEffect::Notice(disabled_extension_teach("/code-swarm", "code-swarm"))
        );

        let cmds = build_extension_slash_commands(&context.extension_items);
        let entry = cmds
            .iter()
            .find(|cmd| cmd.token == "/code-swarm")
            .expect("code-swarm palette entry");
        assert_eq!(entry.extension_id, "code-swarm");
        assert!(entry.summary.contains("reviewer swarm"));
        assert!(!entry.enabled);
        // /code-swarm is the whole surface: it configures reviewers. The
        // agent-only `review` command mints no companion token.
        assert!(!cmds.iter().any(|cmd| cmd.token == "/review"));
    }

    #[test]
    fn extension_slash_arguments_parse_or_reject_never_drop() {
        // Generic slash-argument handling, exercised through a user-invocable
        // command. (It used /review until CodeSwarm became agent-only; the
        // behavior under test was never CodeSwarm-specific.)
        let context = CommandContext {
            extension_slash_commands: vec![ExtensionSlashCommand {
                token: "/catch-up".to_owned(),
                summary: "causal-dag · catch-up".to_owned(),
                extension_id: "causal-dag".to_owned(),
                command: "catch-up".to_owned(),
                enabled: true,
            }],
            ..CommandContext::default()
        };

        // JSON argument parses into the input.
        assert_eq!(
            dispatch_command("/catch-up {\"format\":\"json\"}", &context),
            CommandEffect::Action(CommandAction::ExtensionRun {
                id: "causal-dag".to_owned(),
                command: "catch-up".to_owned(),
                input: serde_json::json!({"format": "json"}),
                raw_args: None,
            })
        );
        // Flag arguments travel to resolve-time ArgSpec parsing.
        assert_eq!(
            dispatch_command("/catch-up --format json", &context),
            CommandEffect::Action(CommandAction::ExtensionRun {
                id: "causal-dag".to_owned(),
                command: "catch-up".to_owned(),
                input: serde_json::json!({}),
                raw_args: Some("--format json".to_owned()),
            })
        );
        // Invalid JSON is an error, not a silent default run.
        assert!(matches!(
            dispatch_command("/catch-up {broken", &context),
            CommandEffect::Message(message) if message.contains("must be JSON")
        ));
        // Free text is a usage error, not a silent default run.
        assert!(matches!(
            dispatch_command("/catch-up json please", &context),
            CommandEffect::Message(message) if message.contains("usage:")
        ));
        // No argument still dispatches with empty input.
        assert_eq!(
            dispatch_command("/catch-up", &context),
            CommandEffect::Action(CommandAction::ExtensionRun {
                id: "causal-dag".to_owned(),
                command: "catch-up".to_owned(),
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
            CommandEffect::Notice(disabled_extension_teach("/catch-up", "causal-dag"))
        );
    }

    #[test]
    fn disabled_extension_run_form_teaches_instead_of_running() {
        // Third entrance (review v2 §14.4): `/extension run <ext>.<cmd>`.
        let context = CommandContext {
            extension_items: vec![ExtensionManagerItem {
                id: "causal-dag".to_owned(),
                display_name: "Causal DAG".to_owned(),
                enabled: false,
                bundled: true,
                materialization: None,
                version: "0.1.0".to_owned(),
                commands: vec![ExtensionCommandItem::user("catch-up")],
                capabilities: vec![],
                audit_status: None,
            }],
            ..CommandContext::default()
        };
        assert_eq!(
            dispatch_command("/extension run causal-dag.catch-up", &context),
            CommandEffect::Notice(disabled_extension_teach(
                "/causal-dag.catch-up",
                "causal-dag"
            ))
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
            CommandEffect::Message(
                "usage: /effort <xsmall|small|medium|large|xlarge|max>".to_owned(),
            )
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
            CommandEffect::Message("unknown command: /dag".to_owned())
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
