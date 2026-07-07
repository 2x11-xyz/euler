use super::super::{
    commands::{theme_choices, CommandContext, EffortChoice, ModelChoice, ResumeItem},
    event_loop::{InputEvent, UiEvent},
    status::TokenUsageSnapshot,
    theme::ThemeChoice,
};
use super::CoreEffect;
use crate::provider_config_runtime;
use anyhow::Result;
use crossterm::event::{self, Event as CrosstermEvent, KeyEvent, KeyEventKind, KeyModifiers};
use euler_core::{EulerHome, ReasoningEffort, SessionStore};
use euler_event::{EventEnvelope, EventKind};
use euler_provider::catalog::{MergedModelCatalog, ModelDescriptor};
use euler_provider::provider_config::{CustomModelConfig, ProviderConfigRegistry};
use serde_json::Value;

pub(super) fn update_token_usage(
    tokens: &mut TokenUsageSnapshot,
    event: &EventEnvelope,
    context_window_tokens: Option<u64>,
) {
    if event.kind.as_str() == EventKind::MODEL_SWITCHED {
        *tokens = TokenUsageSnapshot::default();
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

pub(super) fn command_context(
    model_catalog: &MergedModelCatalog,
    provider: &str,
    model: &str,
    current_effort: ReasoningEffort,
    current_theme: ThemeChoice,
) -> CommandContext {
    // This is called when the bottom surface is rebuilt for session lifecycle
    // transitions, not during frame rendering or palette filtering.
    let provider_config = provider_config_runtime::load_provider_config(
        provider_config_runtime::default_provider_config_path().as_deref(),
    );
    CommandContext {
        model_choices: model_choices(model_catalog, &provider_config.registry, provider, model),
        effort_choices: effort_choices(current_effort),
        theme_choices: theme_choices(current_theme),
        resume_items: resume_items_from_home(),
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

fn resume_items_from_home() -> Vec<ResumeItem> {
    let Ok(home) = EulerHome::resolve() else {
        return Vec::new();
    };
    let Ok(store) = SessionStore::new(home) else {
        return Vec::new();
    };
    let Ok(mut records) = store.list_sessions() else {
        return Vec::new();
    };
    records.sort_by(|left, right| right.id().cmp(left.id()));
    records
        .into_iter()
        .take(20)
        .map(|record| {
            let mut item =
                ResumeItem::new(record.id().to_owned(), record.display_label().to_owned());
            item.status = Some("saved".to_owned());
            item.preview = Some(record.id().to_owned());
            item
        })
        .collect()
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
}
