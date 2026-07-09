use super::super::{
    commands::{
        theme_choices, CheckpointItem, CommandContext, EffortChoice, ModelChoice, ResumeItem,
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
use euler_event::{EventEnvelope, EventKind};
use euler_provider::catalog::{MergedModelCatalog, ModelDescriptor};
use euler_provider::provider_config::{CustomModelConfig, ProviderConfigRegistry};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn update_token_usage(
    tokens: &mut TokenUsageSnapshot,
    event: &EventEnvelope,
    context_window_tokens: Option<u64>,
) {
    if event.kind.as_str() == EventKind::MODEL_SWITCHED {
        *tokens = TokenUsageSnapshot::default();
        return;
    }
    if event.kind.as_str() == EventKind::CANVAS_SNAPSHOT {
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
    let Some(usage) = event.payload.get("usage").and_then(Value::as_object) else {
        return;
    };
    let input_tokens = usage_u64(usage, "input_tokens").unwrap_or(0);
    let output_tokens = usage_u64(usage, "output_tokens").unwrap_or(0);
    tokens.input_tokens = input_tokens;
    tokens.output_tokens = output_tokens;
    tokens.reasoning_tokens = usage_u64(usage, "reasoning_tokens");
    tokens.context_window_tokens = context_window_tokens;
}

pub(super) fn read_terminal_event() -> Result<Option<UiEvent>> {
    let event = match event::read()? {
        CrosstermEvent::Key(key) if key.kind == KeyEventKind::Press => {
            Some(UiEvent::Input(InputEvent::Key(key)))
        }
        CrosstermEvent::Mouse(mouse) => Some(UiEvent::Input(InputEvent::Mouse(mouse))),
        CrosstermEvent::Paste(text) => Some(UiEvent::Input(InputEvent::Paste(text))),
        CrosstermEvent::Resize(width, height) => Some(UiEvent::Resize { width, height }),
        _ => None,
    };
    Ok(event)
}

pub(super) struct CommandContextParts {
    pub current_effort: ReasoningEffort,
    pub current_theme: ThemeChoice,
    pub current_session_id: Option<String>,
    pub checkpoint_items: Vec<CheckpointItem>,
}

pub(super) fn command_context(
    model_catalog: &MergedModelCatalog,
    provider: &str,
    model: &str,
    parts: CommandContextParts,
) -> CommandContext {
    // This is called when the bottom surface is rebuilt for session lifecycle
    // transitions, not during frame rendering or palette filtering.
    let provider_config = provider_config_runtime::load_provider_config(
        provider_config_runtime::default_provider_config_path().as_deref(),
    );
    CommandContext {
        model_choices: model_choices(model_catalog, &provider_config.registry, provider, model),
        effort_choices: effort_choices(parts.current_effort),
        theme_choices: theme_choices(parts.current_theme),
        resume_items: resume_items_from_home(parts.current_session_id.as_deref()),
        checkpoint_items: parts.checkpoint_items,
    }
}

fn effort_choices(current: ReasoningEffort) -> Vec<EffortChoice> {
    ReasoningEffort::ALL
        .into_iter()
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
        model.context_window_tokens(),
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

fn resume_items_from_home(current_session_id: Option<&str>) -> Vec<ResumeItem> {
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
            item.group = Some(
                record
                    .kind()
                    .map_or_else(|| "unknown".to_owned(), |kind| kind.as_str().to_owned()),
            );
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
}
