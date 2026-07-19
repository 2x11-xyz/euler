use super::super::{
    commands::{
        theme_choices, CausalDagStats, CheckpointItem, CommandContext, CompactionSettings,
        EffortChoice, ModelChoice, ResumeItem,
    },
    event_loop::{InputEvent, UiEvent},
    status::TokenUsageSnapshot,
    theme::ThemeChoice,
};
use super::CoreEffect;
use crate::provider_config_runtime;
use anyhow::Result;
use crossterm::event::{self, Event as CrosstermEvent, KeyEvent, KeyEventKind, KeyModifiers};
use euler_core::{EulerHome, ReasoningEffort, SessionRecord, SessionStore};
#[cfg(test)]
use euler_event::object;
use euler_event::{EventEnvelope, EventKind};
use euler_provider::catalog::{MergedModelCatalog, ModelDescriptor};
use euler_provider::provider_config::{CustomModelConfig, ProviderConfigRegistry};
use serde_json::Value;
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

/// Looks up the context window size (in tokens) for the given provider/model
/// pair from the model catalog, if known. Used both mid-turn (via
/// `update_token_usage`) and at session-start points so a fresh session shows
/// `ctx 0%` instead of the `?` unknown fallback whenever the model's context
/// window is known.
pub(super) fn context_window_tokens_for(
    model_catalog: &MergedModelCatalog,
    provider: &str,
    model: &str,
) -> Option<u64> {
    model_catalog
        .provider(provider)
        .and_then(|descriptor| descriptor.models().find(|entry| entry.id() == model))
        .and_then(|entry| entry.effective_context_window_tokens())
}

pub(super) fn update_token_usage(
    tokens: &mut TokenUsageSnapshot,
    event: &EventEnvelope,
    context_window_tokens: Option<u64>,
    primary_agent_id: Option<&str>,
) {
    let is_primary = primary_agent_id == Some(event.agent.as_str());
    if event.kind.as_str() == EventKind::MODEL_SWITCHED && is_primary {
        // Switching models clears the active context reading, but cost is a
        // session-lifetime total just as it is in pi's footer.
        tokens.input_tokens = 0;
        tokens.output_tokens = 0;
        tokens.reasoning_tokens = None;
        tokens.context_window_tokens = context_window_tokens;
        tokens.demoted_items = 0;
        tokens.canvas_retained_bytes = None;
        tokens.canvas_budget_bytes = None;
        tokens.compaction_tier = None;
        return;
    }
    if event.kind.as_str() == EventKind::CANVAS_SNAPSHOT && is_primary {
        tokens.demoted_items = event
            .payload
            .get("demoted_items")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        tokens.canvas_retained_bytes = event.payload.get("retained_bytes").and_then(Value::as_u64);
        tokens.canvas_budget_bytes = event.payload.get("budget_bytes").and_then(Value::as_u64);
        tokens.compaction_tier = event
            .payload
            .get("tier")
            .and_then(Value::as_str)
            .map(str::to_owned);
        return;
    }
    if event.kind.as_str() != EventKind::MODEL_RESULT {
        return;
    }
    let Some(usage) = model_result_usage(event) else {
        return;
    };
    if is_primary {
        if let ModelResultUsage::Reported {
            input_tokens,
            output_tokens,
            reasoning_tokens,
        } = usage
        {
            tokens.input_tokens = input_tokens;
            tokens.output_tokens = output_tokens;
            tokens.reasoning_tokens = reasoning_tokens;
            tokens.context_window_tokens = context_window_tokens;
        }
    }
    match persisted_model_cost_picos(event) {
        Some(picos) => {
            tokens.session_cost_picos += u128::from(picos);
            tokens.priced_calls = tokens.priced_calls.saturating_add(1);
        }
        None => tokens.unpriced_calls = tokens.unpriced_calls.saturating_add(1),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModelResultUsage {
    Unavailable,
    Reported {
        input_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: Option<u64>,
    },
}

fn model_result_usage(event: &EventEnvelope) -> Option<ModelResultUsage> {
    event
        .payload
        .get("provider")?
        .as_str()
        .filter(|value| !value.is_empty())?;
    event
        .payload
        .get("model")?
        .as_str()
        .filter(|value| !value.is_empty())?;
    match event.payload.get("usage")? {
        Value::Null => Some(ModelResultUsage::Unavailable),
        Value::Object(usage) => Some(ModelResultUsage::Reported {
            input_tokens: usage_u64(usage, "input_tokens")?,
            output_tokens: usage_u64(usage, "output_tokens")?,
            reasoning_tokens: optional_usage_u64(usage, "reasoning_tokens")?,
        }),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct PricedUsage {
    input_tokens: u64,
    output_tokens: u64,
    uncached_input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_5m_tokens: u64,
    cache_write_1h_tokens: u64,
}

impl PricedUsage {
    fn from_event(event: &EventEnvelope) -> Option<Self> {
        let usage = event.payload.get("usage")?.as_object()?;
        let value = Self {
            input_tokens: usage_u64(usage, "input_tokens")?,
            output_tokens: usage_u64(usage, "output_tokens")?,
            uncached_input_tokens: usage_u64(usage, "uncached_input_tokens")?,
            cache_read_tokens: usage_u64(usage, "cached_tokens")?,
            cache_write_5m_tokens: usage_u64(usage, "cache_write_5m_tokens")?,
            cache_write_1h_tokens: usage_u64(usage, "cache_write_1h_tokens")?,
        };
        (value.input_bucket_sum()? == value.input_tokens).then_some(value)
    }

    fn input_bucket_sum(self) -> Option<u64> {
        self.uncached_input_tokens
            .checked_add(self.cache_read_tokens)?
            .checked_add(self.cache_write_5m_tokens)?
            .checked_add(self.cache_write_1h_tokens)
    }
}

fn persisted_model_cost_picos(event: &EventEnvelope) -> Option<u64> {
    let cost = event.payload.get("cost")?.as_object()?;
    const COST_FIELDS: [&str; 10] = [
        "schema_version",
        "currency",
        "unit",
        "input_picos",
        "output_picos",
        "cache_read_picos",
        "cache_write_5m_picos",
        "cache_write_1h_picos",
        "total_picos",
        "pricing",
    ];
    if !has_exact_fields(cost, &COST_FIELDS) {
        return None;
    }
    if cost.get("schema_version")?.as_u64()? != 1
        || cost.get("currency")?.as_str()? != "USD"
        || cost.get("unit")?.as_str()? != "picodollar"
    {
        return None;
    }
    let usage = PricedUsage::from_event(event)?;
    let pricing = validate_persisted_pricing(event, cost.get("pricing")?.as_object()?)?;
    if pricing
        .tier_input_tokens_above
        .is_some_and(|threshold| usage.input_tokens <= threshold)
    {
        return None;
    }
    let sum = validate_cost_components(cost, usage, pricing.rates)?;
    if sum != cost.get("total_picos")?.as_u64()? {
        return None;
    }
    Some(sum)
}

#[derive(Clone, Copy)]
struct PersistedRates {
    input: u64,
    output: u64,
    cache_read: Option<u64>,
    cache_write_5m: Option<u64>,
    cache_write_1h: Option<u64>,
}

#[derive(Clone, Copy)]
struct PersistedPricing {
    rates: PersistedRates,
    tier_input_tokens_above: Option<u64>,
}

fn validate_persisted_pricing(
    event: &EventEnvelope,
    pricing: &serde_json::Map<String, Value>,
) -> Option<PersistedPricing> {
    let provider = event.payload.get("provider")?.as_str()?;
    let model = event.payload.get("model")?.as_str()?;
    let source = pricing.get("source")?.as_str()?;
    let source_id = pricing.get("source_id")?.as_str()?;
    const PRICING_FIELDS: [&str; 6] = [
        "provider",
        "model",
        "source",
        "source_id",
        "rates",
        "tier_input_tokens_above",
    ];
    if has_unknown_fields(pricing, &PRICING_FIELDS) || !matches!(pricing.len(), 5 | 6) {
        return None;
    }
    let tier_input_tokens_above = optional_positive_u64(pricing, "tier_input_tokens_above")?;
    let rates = pricing.get("rates")?.as_object()?;
    const RATE_FIELDS: [&str; 5] = [
        "input_picos_per_token",
        "output_picos_per_token",
        "cache_read_picos_per_token",
        "cache_write_5m_picos_per_token",
        "cache_write_1h_picos_per_token",
    ];
    if has_unknown_fields(rates, &RATE_FIELDS)
        || rates.values().any(|value| value.as_u64().is_none())
    {
        return None;
    }
    if pricing.get("provider")?.as_str()? != provider
        || pricing.get("model")?.as_str()? != model
        || !valid_pricing_source(source, source_id)
    {
        return None;
    }
    Some(PersistedPricing {
        rates: PersistedRates {
            input: rates.get("input_picos_per_token")?.as_u64()?,
            output: rates.get("output_picos_per_token")?.as_u64()?,
            cache_read: optional_rate(rates, "cache_read_picos_per_token")?,
            cache_write_5m: optional_rate(rates, "cache_write_5m_picos_per_token")?,
            cache_write_1h: optional_rate(rates, "cache_write_1h_picos_per_token")?,
        },
        tier_input_tokens_above,
    })
}

fn validate_cost_components(
    cost: &serde_json::Map<String, Value>,
    usage: PricedUsage,
    rates: PersistedRates,
) -> Option<u64> {
    let components = [
        (
            "input_picos",
            usage.uncached_input_tokens,
            Some(rates.input),
        ),
        ("output_picos", usage.output_tokens, Some(rates.output)),
        (
            "cache_read_picos",
            usage.cache_read_tokens,
            rates.cache_read,
        ),
        (
            "cache_write_5m_picos",
            usage.cache_write_5m_tokens,
            rates.cache_write_5m,
        ),
        (
            "cache_write_1h_picos",
            usage.cache_write_1h_tokens,
            rates.cache_write_1h,
        ),
    ];
    components
        .into_iter()
        .try_fold(0_u64, |total, (field, tokens, rate)| {
            let expected = component_cost(tokens, rate)?;
            (cost.get(field)?.as_u64()? == expected).then(|| total.checked_add(expected))?
        })
}

fn component_cost(tokens: u64, rate: Option<u64>) -> Option<u64> {
    if tokens == 0 {
        Some(0)
    } else {
        tokens.checked_mul(rate?)
    }
}

fn optional_rate(object: &serde_json::Map<String, Value>, field: &str) -> Option<Option<u64>> {
    match object.get(field) {
        Some(value) => Some(Some(value.as_u64()?)),
        None => Some(None),
    }
}

fn optional_positive_u64(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Option<Option<u64>> {
    match object.get(field) {
        Some(value) => Some(Some(value.as_u64().filter(|value| *value > 0)?)),
        None => Some(None),
    }
}

fn optional_usage_u64(object: &serde_json::Map<String, Value>, field: &str) -> Option<Option<u64>> {
    optional_rate(object, field)
}

fn valid_pricing_source(source: &str, source_id: &str) -> bool {
    match source {
        "local" => is_lower_hex_digest(source_id),
        "official" => is_catalog_release_id(source_id),
        _ => false,
    }
}

fn is_lower_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_catalog_release_id(value: &str) -> bool {
    let Some(value) = value.strip_prefix("catalog-v1-") else {
        return false;
    };
    let Some((timestamp, digest)) = value.split_once('-') else {
        return false;
    };
    let timestamp = timestamp.as_bytes();
    timestamp.len() == 16
        && timestamp[..8].iter().all(u8::is_ascii_digit)
        && timestamp[8] == b't'
        && timestamp[9..15].iter().all(u8::is_ascii_digit)
        && timestamp[15] == b'z'
        && is_lower_hex_digest(digest)
}

fn has_exact_fields(object: &serde_json::Map<String, Value>, fields: &[&str]) -> bool {
    object.len() == fields.len() && !has_unknown_fields(object, fields)
}

fn has_unknown_fields(object: &serde_json::Map<String, Value>, fields: &[&str]) -> bool {
    object.keys().any(|field| !fields.contains(&field.as_str()))
}

pub(super) fn read_terminal_event() -> Result<Option<UiEvent>> {
    let event = match event::read()? {
        CrosstermEvent::Key(key) if key.kind == KeyEventKind::Press => {
            Some(UiEvent::Input(InputEvent::Key(key)))
        }
        CrosstermEvent::Mouse(mouse) => Some(UiEvent::Input(InputEvent::Mouse(mouse))),
        CrosstermEvent::Paste(text) => Some(UiEvent::Input(InputEvent::Paste(text))),
        CrosstermEvent::Resize(width, height) => Some(UiEvent::Resize { width, height }),
        CrosstermEvent::FocusGained => Some(UiEvent::FocusChanged(true)),
        CrosstermEvent::FocusLost => Some(UiEvent::FocusChanged(false)),
        _ => None,
    };
    Ok(event)
}

pub(super) struct CommandContextParts {
    pub current_effort: ReasoningEffort,
    pub current_theme: ThemeChoice,
    pub checkpoint_items: Vec<CheckpointItem>,
    pub extension_items: Vec<super::super::commands::ExtensionManagerItem>,
    pub extension_slash_commands: Vec<super::super::commands::ExtensionSlashCommand>,
    pub code_swarm_models: Vec<String>,
    pub causal_dag_stats: Option<CausalDagStats>,
    pub compaction: CompactionSettings,
}

pub(super) fn command_context(
    model_catalog: &MergedModelCatalog,
    provider: &str,
    model: &str,
    authenticated_providers: &BTreeSet<String>,
    parts: CommandContextParts,
) -> CommandContext {
    // This is called when the bottom surface is rebuilt for session lifecycle
    // transitions, not during frame rendering or palette filtering.
    let provider_config = provider_config_runtime::load_provider_config(
        provider_config_runtime::default_provider_config_path().as_deref(),
    );
    let model_choices = model_choices(model_catalog, &provider_config.registry, provider, model);
    // The reviewer-model picker must not offer a target that will burn a
    // spawn slot to discover it isn't authenticated (#58): filter to
    // providers `validate_auth` accepted when the session was last Idle.
    // The general `/model` picker keeps the full list — switching to an
    // unauthenticated provider there is a normal way to be prompted to
    // /login. The caller passes a cached snapshot so rebuilds while the
    // session is checked out onto the worker thread (mid-turn, after
    // submission) never shrink the picker to empty.
    let code_swarm_model_choices = model_choices
        .iter()
        .filter(|choice| authenticated_providers.contains(&choice.provider))
        .cloned()
        .collect();
    CommandContext {
        model_choices,
        code_swarm_model_choices,
        effort_choices: effort_choices(model_catalog, parts.current_effort, provider, model),
        theme_choices: theme_choices(parts.current_theme),
        checkpoint_items: parts.checkpoint_items,
        extension_items: parts.extension_items,
        extension_slash_commands: parts.extension_slash_commands,
        code_swarm_models: parts.code_swarm_models,
        causal_dag_stats: parts.causal_dag_stats,
        compaction: parts.compaction,
    }
}

pub(super) fn causal_dag_stats_from_events(
    events: &[EventEnvelope],
    session_id: &str,
) -> CausalDagStats {
    let metadata = events.iter().rev().find_map(|event| {
        if event.kind.as_str() != EventKind::EXTENSION_ARTIFACT
            || event.payload.get("extension_id").and_then(Value::as_str) != Some("causal-dag")
        {
            return None;
        }
        let metadata = event.payload.get("metadata")?.as_object()?;
        (metadata.get("schema").and_then(Value::as_str) == Some("euler.causal_dag.v3"))
            .then_some(metadata)
    });
    CausalDagStats {
        session_id: session_id.to_owned(),
        node_count: metadata
            .and_then(|value| value.get("node_count"))
            .and_then(Value::as_u64)
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or_default(),
        cross_arc_count: metadata
            .and_then(|value| {
                value
                    .get("annotation_edge_count")
                    .or_else(|| value.get("cross_arc_count"))
            })
            .and_then(Value::as_u64)
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or_default(),
    }
}

fn effort_choices(
    catalog: &MergedModelCatalog,
    current: ReasoningEffort,
    provider: &str,
    model: &str,
) -> Vec<EffortChoice> {
    catalog
        .supported_reasoning_efforts(provider, model)
        .iter()
        .copied()
        .map(|effort| EffortChoice::new(effort, current))
        .collect()
}

fn model_choices(
    catalog: &MergedModelCatalog,
    custom_providers: &ProviderConfigRegistry,
    current_provider: &str,
    current_model: &str,
) -> Vec<ModelChoice> {
    let mut choices = catalog
        .providers()
        .flat_map(|provider| {
            provider.models().map(|model| {
                catalog_model_choice(provider.id(), model, current_provider, current_model)
            })
        })
        .chain(custom_model_choices(
            custom_providers,
            current_provider,
            current_model,
        ))
        .collect::<Vec<_>>();
    ensure_current_model_choice(&mut choices, current_provider, current_model);
    choices
}

fn custom_model_choices(
    custom_providers: &ProviderConfigRegistry,
    current_provider: &str,
    current_model: &str,
) -> Vec<ModelChoice> {
    let mut providers = custom_providers.providers().collect::<Vec<_>>();
    providers.sort_by(|left, right| left.id.cmp(&right.id));
    providers
        .into_iter()
        .flat_map(|provider| {
            provider.models.values().map(|model| {
                custom_model_choice(&provider.id, model, current_provider, current_model)
            })
        })
        .collect()
}

fn catalog_model_choice(
    provider: &str,
    model: &ModelDescriptor,
    current_provider: &str,
    current_model: &str,
) -> ModelChoice {
    let mut choice = ModelChoice::with_metadata(
        provider,
        model.id(),
        model.effective_context_window_tokens(),
        model.supports_reasoning(),
    );
    choice.current = provider == current_provider && model.id() == current_model;
    choice
}

fn custom_model_choice(
    provider: &str,
    model: &CustomModelConfig,
    current_provider: &str,
    current_model: &str,
) -> ModelChoice {
    let mut choice = ModelChoice::with_metadata(
        provider,
        &model.id,
        model.context_window_tokens,
        model.supports_reasoning,
    );
    choice.current = provider == current_provider && model.id == current_model;
    choice
}

fn ensure_current_model_choice(
    choices: &mut Vec<ModelChoice>,
    current_provider: &str,
    current_model: &str,
) {
    if choices
        .iter()
        .any(|choice| choice.provider == current_provider && choice.model == current_model)
    {
        return;
    }
    choices.push(ModelChoice::current(current_provider, current_model));
}

/// Lists every session record under the Euler home — full-store disk work —
/// so this must only run on user-initiated picker opens, never inside
/// `command_context` rebuilds (which sit on the submit/turn-end hot path).
pub(super) fn resume_items_from_home(current_session_id: Option<&str>) -> Vec<ResumeItem> {
    let Ok(home) = EulerHome::resolve() else {
        return Vec::new();
    };
    let Ok(store) = SessionStore::new(home) else {
        return Vec::new();
    };
    let Ok(records) = store.list_sessions() else {
        return Vec::new();
    };
    resume_items_from_records(records, current_session_id)
}

fn resume_items_from_records(
    records: Vec<SessionRecord>,
    current_session_id: Option<&str>,
) -> Vec<ResumeItem> {
    resume_items_from_records_at(records, current_session_id, now_unix_ms())
}

fn resume_items_from_records_at(
    mut records: Vec<SessionRecord>,
    current_session_id: Option<&str>,
    now_ms: u64,
) -> Vec<ResumeItem> {
    records.sort_by(|left, right| {
        right
            .updated_at_ms()
            .cmp(&left.updated_at_ms())
            .then_with(|| right.id().cmp(left.id()))
    });
    records
        .into_iter()
        .filter(|record| Some(record.id()) != current_session_id)
        .take(20)
        .map(|record| {
            let mut item = ResumeItem::new(record.id().to_owned(), session_resume_label(&record));
            item.status = Some(relative_age(record.updated_at_ms(), now_ms));
            item.preview = Some(resume_detail(&record));
            // Spec §5.10 launch-kind group headers: interactive → tui, non-interactive → exec.
            item.group = Some(match record.kind() {
                Some(euler_core::SessionKind::Interactive) => "tui".to_owned(),
                Some(euler_core::SessionKind::NonInteractive) => "exec".to_owned(),
                None => "unknown".to_owned(),
            });
            item
        })
        .collect()
}

pub(super) fn session_resume_label(record: &SessionRecord) -> String {
    record
        .name()
        .or_else(|| record.title())
        .map_or_else(|| "Untitled session".to_owned(), str::to_owned)
}

fn resume_detail(record: &SessionRecord) -> String {
    let mut parts = vec![record.id().to_owned()];
    if let Some(root) = record.root() {
        parts.push(root.display().to_string());
    }
    parts.join("  ")
}

fn relative_age(updated_at_ms: u64, now_ms: u64) -> String {
    let elapsed_secs = now_ms.saturating_sub(updated_at_ms) / 1000;
    if elapsed_secs < 60 {
        return "just now".to_owned();
    }
    let minutes = elapsed_secs / 60;
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    format!("{}d ago", hours / 24)
}

fn now_unix_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    millis.min(u128::from(u64::MAX)) as u64
}

pub(super) fn session_root_status_path() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// Detects the current git branch by reading `.git/HEAD` directly (no git2
/// dependency, no subprocess). Returns the branch name for `ref:
/// refs/heads/<name>`, the first 7 hex chars of the commit for a detached
/// HEAD, or `None` if there is no readable `.git/HEAD`.
pub(super) fn detect_git_branch(workspace_root: &std::path::Path) -> Option<String> {
    let head_path = workspace_root.join(".git").join("HEAD");
    let contents = std::fs::read_to_string(head_path).ok()?;
    let contents = contents.trim();
    if let Some(branch_ref) = contents.strip_prefix("ref: refs/heads/") {
        if branch_ref.is_empty() {
            return None;
        }
        return Some(branch_ref.to_owned());
    }
    let is_hex = !contents.is_empty() && contents.chars().all(|c| c.is_ascii_hexdigit());
    if is_hex {
        return Some(contents.chars().take(7).collect());
    }
    None
}

pub(super) fn merge_effects(left: CoreEffect, right: CoreEffect) -> CoreEffect {
    match (left, right) {
        (CoreEffect::Quit, _) | (_, CoreEffect::Quit) => CoreEffect::Quit,
        (CoreEffect::ReplayHistoryWithScrollbackPurge, _)
        | (_, CoreEffect::ReplayHistoryWithScrollbackPurge) => {
            CoreEffect::ReplayHistoryWithScrollbackPurge
        }
        (CoreEffect::ReplayHistory, _) | (_, CoreEffect::ReplayHistory) => {
            CoreEffect::ReplayHistory
        }
        (CoreEffect::TerminalClipboard, _) | (_, CoreEffect::TerminalClipboard) => {
            CoreEffect::TerminalClipboard
        }
        (CoreEffect::ThemeChanged, _) | (_, CoreEffect::ThemeChanged) => CoreEffect::ThemeChanged,
        (CoreEffect::Render, _) | (_, CoreEffect::Render) => CoreEffect::Render,
        _ => CoreEffect::None,
    }
}

pub(super) fn is_copy_key(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.modifiers.contains(KeyModifiers::SHIFT)
}

fn usage_u64(usage: &serde_json::Map<String, Value>, field: &str) -> Option<u64> {
    usage.get(field).and_then(Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_usage_accumulates_persisted_cost_and_marks_unpriced_history() {
        let mut tokens = TokenUsageSnapshot::default();
        let usage = TestUsage::plain(1_000, 100);
        let priced = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::MODEL_RESULT,
            object([
                ("provider", "openai".into()),
                ("model", "gpt-5.5".into()),
                ("usage", test_usage(usage)),
                ("cost", test_cost("openai", "gpt-5.5", usage)),
            ]),
        );
        let unpriced = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::MODEL_RESULT,
            object([
                ("provider", "fixture".into()),
                ("model", "echo".into()),
                (
                    "usage",
                    serde_json::json!({"input_tokens": 5, "output_tokens": 2}),
                ),
            ]),
        );

        update_token_usage(&mut tokens, &priced, Some(1_000_000), Some("root"));
        update_token_usage(&mut tokens, &unpriced, None, Some("root"));

        assert_eq!(tokens.session_cost_picos, 2_300);
        assert_eq!(tokens.priced_calls, 1);
        assert_eq!(tokens.unpriced_calls, 1);
    }

    #[test]
    fn token_usage_marks_missing_provider_usage_as_unpriced() {
        let mut tokens = TokenUsageSnapshot::default();
        let event = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::MODEL_RESULT,
            object([
                ("provider", "openai".into()),
                ("model", "gpt-5.5".into()),
                ("usage", Value::Null),
            ]),
        );

        update_token_usage(&mut tokens, &event, Some(1_000_000), Some("root"));

        assert_eq!(tokens.priced_calls, 0);
        assert_eq!(tokens.unpriced_calls, 1);
    }

    #[test]
    fn token_usage_rejects_cost_for_a_different_model() {
        let mut tokens = TokenUsageSnapshot::default();
        let usage = TestUsage::plain(5, 2);
        let event = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::MODEL_RESULT,
            object([
                ("provider", "openai".into()),
                ("model", "gpt-5.5".into()),
                ("usage", test_usage(usage)),
                ("cost", test_cost("openai", "gpt-other", usage)),
            ]),
        );

        update_token_usage(&mut tokens, &event, Some(1_000_000), Some("root"));

        assert_eq!(tokens.session_cost_picos, 0);
        assert_eq!(tokens.priced_calls, 0);
        assert_eq!(tokens.unpriced_calls, 1);
    }

    #[test]
    fn companion_cost_counts_without_replacing_primary_context() {
        let mut tokens = TokenUsageSnapshot::default();
        let root_usage = TestUsage::plain(10, 1);
        let child_usage = TestUsage::plain(4, 2);
        for (agent, usage, context_window) in [
            ("root", root_usage, 1_000_000),
            ("agent-child", child_usage, 200_000),
        ] {
            let event = EventEnvelope::new(
                "session",
                agent,
                None,
                EventKind::MODEL_RESULT,
                object([
                    ("provider", "openai".into()),
                    ("model", "gpt-5.5".into()),
                    ("usage", test_usage(usage)),
                    ("cost", test_cost("openai", "gpt-5.5", usage)),
                ]),
            );
            update_token_usage(&mut tokens, &event, Some(context_window), Some("root"));
        }

        assert_eq!(tokens.input_tokens, 10);
        assert_eq!(tokens.output_tokens, 1);
        assert_eq!(tokens.context_window_tokens, Some(1_000_000));
        assert_eq!(tokens.session_cost_picos, 37);
        assert_eq!(tokens.priced_calls, 2);
    }

    #[test]
    fn malformed_model_result_does_not_create_an_unpriced_call() {
        let mut tokens = TokenUsageSnapshot::default();
        let event = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::MODEL_RESULT,
            object([
                ("provider", "openai".into()),
                ("model", "gpt-5.5".into()),
                ("usage", serde_json::json!({"input_tokens": 5})),
            ]),
        );

        update_token_usage(&mut tokens, &event, None, Some("root"));

        assert_eq!(tokens.unpriced_calls, 0);
    }

    #[test]
    fn persisted_cost_must_match_usage_rates_and_selected_tier() {
        let usage = TestUsage::plain(5, 2);
        let mut corrupted_component = priced_event(usage);
        corrupted_component.payload["cost"]["input_picos"] = 11.into();
        let mut impossible_tier = priced_event(usage);
        impossible_tier.payload["cost"]["pricing"]["tier_input_tokens_above"] = 5.into();

        for event in [corrupted_component, impossible_tier] {
            let mut tokens = TokenUsageSnapshot::default();
            update_token_usage(&mut tokens, &event, None, Some("root"));
            assert_eq!(tokens.priced_calls, 0);
            assert_eq!(tokens.unpriced_calls, 1);
        }
    }

    #[test]
    fn nonzero_cache_bucket_requires_its_persisted_rate() {
        let usage = TestUsage {
            uncached: 4,
            output: 2,
            cache_read: 1,
            cache_write_5m: 0,
            cache_write_1h: 0,
        };
        let mut event = priced_event(usage);
        event.payload["cost"]["pricing"]["rates"]
            .as_object_mut()
            .expect("rates")
            .remove("cache_read_picos_per_token");
        let mut tokens = TokenUsageSnapshot::default();

        update_token_usage(&mut tokens, &event, None, Some("root"));

        assert_eq!(tokens.priced_calls, 0);
        assert_eq!(tokens.unpriced_calls, 1);
    }

    #[test]
    fn persisted_price_identity_must_match_its_source_wire_format() {
        let usage = TestUsage::plain(5, 2);
        let mut malformed_official = priced_event(usage);
        malformed_official.payload["cost"]["pricing"]["source_id"] = "catalog-v1-test".into();
        let mut malformed_local = priced_event(usage);
        malformed_local.payload["cost"]["pricing"]["source"] = "local".into();
        malformed_local.payload["cost"]["pricing"]["source_id"] = "ABC123".into();
        let mut valid_local = priced_event(usage);
        valid_local.payload["cost"]["pricing"]["source"] = "local".into();
        valid_local.payload["cost"]["pricing"]["source_id"] = "0".repeat(64).into();

        for event in [malformed_official, malformed_local] {
            let mut tokens = TokenUsageSnapshot::default();
            update_token_usage(&mut tokens, &event, None, Some("root"));
            assert_eq!(tokens.unpriced_calls, 1);
        }
        let mut tokens = TokenUsageSnapshot::default();
        update_token_usage(&mut tokens, &valid_local, None, Some("root"));
        assert_eq!(tokens.priced_calls, 1);
    }

    #[derive(Clone, Copy)]
    struct TestUsage {
        uncached: u64,
        output: u64,
        cache_read: u64,
        cache_write_5m: u64,
        cache_write_1h: u64,
    }

    impl TestUsage {
        fn plain(input: u64, output: u64) -> Self {
            Self {
                uncached: input,
                output,
                cache_read: 0,
                cache_write_5m: 0,
                cache_write_1h: 0,
            }
        }

        fn input(self) -> u64 {
            self.uncached + self.cache_read + self.cache_write_5m + self.cache_write_1h
        }
    }

    fn test_usage(usage: TestUsage) -> Value {
        serde_json::json!({
            "input_tokens": usage.input(),
            "output_tokens": usage.output,
            "uncached_input_tokens": usage.uncached,
            "cached_tokens": usage.cache_read,
            "cache_write_5m_tokens": usage.cache_write_5m,
            "cache_write_1h_tokens": usage.cache_write_1h
        })
    }

    fn priced_event(usage: TestUsage) -> EventEnvelope {
        EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::MODEL_RESULT,
            object([
                ("provider", "openai".into()),
                ("model", "gpt-5.5".into()),
                ("usage", test_usage(usage)),
                ("cost", test_cost("openai", "gpt-5.5", usage)),
            ]),
        )
    }

    fn test_cost(provider: &str, model: &str, usage: TestUsage) -> Value {
        let input_picos = usage.uncached * 2;
        let output_picos = usage.output * 3;
        let cache_read_picos = usage.cache_read * 5;
        let cache_write_5m_picos = usage.cache_write_5m * 7;
        let cache_write_1h_picos = usage.cache_write_1h * 11;
        let total_picos = input_picos
            + output_picos
            + cache_read_picos
            + cache_write_5m_picos
            + cache_write_1h_picos;
        serde_json::json!({
            "schema_version": 1,
            "currency": "USD",
            "unit": "picodollar",
            "input_picos": input_picos,
            "output_picos": output_picos,
            "cache_read_picos": cache_read_picos,
            "cache_write_5m_picos": cache_write_5m_picos,
            "cache_write_1h_picos": cache_write_1h_picos,
            "total_picos": total_picos,
            "pricing": {
                "provider": provider,
                "model": model,
                "source": "official",
                "source_id": "catalog-v1-20260718t000000z-218346974a6da9255e39f79e4a59912de5d344d7144c1dee597424b173c7f73f",
                "rates": {
                    "input_picos_per_token": 2,
                    "output_picos_per_token": 3,
                    "cache_read_picos_per_token": 5,
                    "cache_write_5m_picos_per_token": 7,
                    "cache_write_1h_picos_per_token": 11
                }
            }
        })
    }

    #[test]
    fn causal_dag_stats_read_the_latest_graph_artifact_not_a_derived_export() {
        let graph = EventEnvelope::new(
            "session-1",
            "agent-1",
            None,
            EventKind::EXTENSION_ARTIFACT,
            object([
                ("extension_id", "causal-dag".into()),
                (
                    "metadata",
                    serde_json::json!({
                        "schema": "euler.causal_dag.v3",
                        "node_count": 35,
                        "annotation_edge_count": 7
                    }),
                ),
            ]),
        );
        let derived = EventEnvelope::new(
            "session-1",
            "agent-1",
            None,
            EventKind::EXTENSION_ARTIFACT,
            object([
                ("extension_id", "causal-dag".into()),
                (
                    "metadata",
                    serde_json::json!({
                        "schema": "euler.causal_dag.export.v1",
                        "node_count": 1,
                        "cross_arc_count": 0
                    }),
                ),
            ]),
        );

        assert_eq!(
            causal_dag_stats_from_events(&[graph, derived], "session-1"),
            CausalDagStats {
                session_id: "session-1".to_owned(),
                node_count: 35,
                cross_arc_count: 7,
            }
        );
    }

    #[test]
    fn model_choices_include_builtin_local_and_custom_models() {
        let (catalog, catalog_warnings) = MergedModelCatalog::with_local_json(
            r#"{
              "providers": {
                "openrouter": {
                  "models": [
                    { "id": "openai/gpt-4.1-mini", "display_name": "GPT 4.1 mini" },
                    { "id": "anthropic/claude-sonnet-4", "display_name": "Sonnet 4" }
                  ]
                }
              }
            }"#,
        );
        let (custom_providers, provider_warnings) = ProviderConfigRegistry::with_json(
            r#"{
              "providers": {
                "local-ollama": {
                  "api_family": "openai_chat_completions",
                  "base_url": "http://localhost:11434/v1",
                  "api_key": "SHOULD_NOT_LEAK",
                  "models": [
                    { "id": "qwen3:32b", "display_name": "Qwen local" }
                  ]
                }
              }
            }"#,
        );
        assert!(catalog_warnings.is_empty());
        assert!(provider_warnings.is_empty());

        let choices = model_choices(
            &catalog,
            &custom_providers,
            "openrouter",
            "anthropic/claude-sonnet-4",
        );

        assert!(choices.iter().any(|choice| {
            choice.provider == "anthropic"
                && choice.model == "claude-sonnet-5"
                && choice.label == "anthropic::claude-sonnet-5 — 1M ctx, reasoning"
        }));
        assert!(choices.iter().any(|choice| {
            choice.provider == "openrouter"
                && choice.model == "openai/gpt-4.1-mini"
                && choice.label == "openrouter::openai/gpt-4.1-mini"
                && !choice.current
        }));
        assert!(choices.iter().any(|choice| {
            choice.provider == "openrouter"
                && choice.model == "anthropic/claude-sonnet-4"
                && choice.label == "openrouter::anthropic/claude-sonnet-4"
                && choice.current
        }));
        assert!(choices.iter().any(|choice| {
            choice.provider == "local-ollama"
                && choice.model == "qwen3:32b"
                && choice.label == "local-ollama::qwen3:32b"
        }));
        assert!(!format!("{choices:?}").contains("SHOULD_NOT_LEAK"));
    }

    #[test]
    fn chatgpt_56_usage_and_picker_use_effective_context_window() {
        let catalog = MergedModelCatalog::built_in();

        assert_eq!(
            context_window_tokens_for(&catalog, "chatgpt", "gpt-5.6-terra"),
            Some(353_400)
        );
        let choices = model_choices(
            &catalog,
            &ProviderConfigRegistry::default(),
            "chatgpt",
            "gpt-5.6-terra",
        );
        let terra = choices
            .iter()
            .find(|choice| choice.provider == "chatgpt" && choice.model == "gpt-5.6-terra")
            .expect("Terra choice");
        assert!(terra.label.contains("353K ctx"), "{}", terra.label);
    }

    #[test]
    fn model_choices_keep_active_explicit_model_when_catalog_lacks_it() {
        let choices = model_choices(
            &MergedModelCatalog::built_in(),
            &ProviderConfigRegistry::default(),
            "openrouter",
            "new/model-not-in-local-catalog",
        );

        let current = choices
            .iter()
            .filter(|choice| choice.current)
            .collect::<Vec<_>>();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].provider, "openrouter");
        assert_eq!(current[0].model, "new/model-not-in-local-catalog");
        assert_eq!(
            current[0].label,
            "openrouter::new/model-not-in-local-catalog"
        );
    }

    #[test]
    fn effort_choices_expose_max_only_for_supported_models() {
        let catalog = MergedModelCatalog::built_in();
        let standard = effort_choices(&catalog, ReasoningEffort::Medium, "chatgpt", "gpt-5.5");
        let gpt_5_6 = effort_choices(&catalog, ReasoningEffort::Max, "chatgpt", "gpt-5.6-sol");

        assert_eq!(
            standard
                .iter()
                .map(|choice| choice.effort.as_str())
                .collect::<Vec<_>>(),
            ["xsmall", "small", "medium", "large", "xlarge"]
        );
        assert_eq!(
            gpt_5_6
                .iter()
                .map(|choice| (choice.effort.as_str(), choice.current))
                .collect::<Vec<_>>(),
            [
                ("xsmall", false),
                ("small", false),
                ("medium", false),
                ("large", false),
                ("xlarge", false),
                ("max", true),
            ]
        );
    }

    #[test]
    fn resume_items_exclude_current_session() {
        let temp = tempfile::tempdir().expect("temp dir");
        let home = EulerHome::from_root(temp.path().join(".euler")).expect("home");
        let store = SessionStore::new(home).expect("store");
        let current = store.create_session().expect("current session");
        let prior = store.create_session().expect("prior session");

        let items =
            resume_items_from_records(store.list_sessions().expect("sessions"), Some(current.id()));

        assert_eq!(items.len(), 1);
        assert!(!items.iter().any(|item| item.id == current.id()));
        assert!(items.iter().any(|item| item.id == prior.id()));
    }

    #[test]
    fn relative_age_uses_compact_buckets() {
        assert_eq!(relative_age(1_000, 1_000), "just now");
        assert_eq!(relative_age(0, 59_000), "just now");
        assert_eq!(relative_age(0, 60_000), "1m ago");
        assert_eq!(relative_age(0, 3_600_000), "1h ago");
        assert_eq!(relative_age(0, 86_400_000), "1d ago");
        assert_eq!(relative_age(0, 900_000_000), "10d ago");
    }

    #[test]
    fn detect_git_branch_reads_branch_ref() {
        let temp = tempfile::tempdir().expect("temp dir");
        let git_dir = temp.path().join(".git");
        std::fs::create_dir_all(&git_dir).expect("mkdir .git");
        std::fs::write(
            git_dir.join("HEAD"),
            "ref: refs/heads/feat/warm-ledger-tui\n",
        )
        .expect("write HEAD");

        assert_eq!(
            detect_git_branch(temp.path()),
            Some("feat/warm-ledger-tui".to_owned())
        );
    }

    #[test]
    fn detect_git_branch_reads_short_hash_when_detached() {
        let temp = tempfile::tempdir().expect("temp dir");
        let git_dir = temp.path().join(".git");
        std::fs::create_dir_all(&git_dir).expect("mkdir .git");
        std::fs::write(
            git_dir.join("HEAD"),
            "1234567890abcdef1234567890abcdef12345678\n",
        )
        .expect("write HEAD");

        assert_eq!(detect_git_branch(temp.path()), Some("1234567".to_owned()));
    }

    #[test]
    fn detect_git_branch_returns_none_without_git_dir() {
        let temp = tempfile::tempdir().expect("temp dir");

        assert_eq!(detect_git_branch(temp.path()), None);
    }

    #[test]
    fn detect_git_branch_returns_none_for_malformed_head() {
        let temp = tempfile::tempdir().expect("temp dir");
        let git_dir = temp.path().join(".git");
        std::fs::create_dir_all(&git_dir).expect("mkdir .git");
        std::fs::write(git_dir.join("HEAD"), "not a valid head\n").expect("write HEAD");

        assert_eq!(detect_git_branch(temp.path()), None);
    }
}
