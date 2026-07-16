use super::args::ProviderOptions;
use super::command::RunArgs;
use super::model_resolution::{
    canonical_model_preference, parse_known_provider_id, parse_provider_id,
};
use anyhow::{anyhow, Result};
use euler_core::ModelTarget;
use euler_provider::anthropic::AnthropicProvider;
use euler_provider::catalog::{MergedModelCatalog, BUILTIN_PROVIDERS};
use euler_provider::chatgpt::ChatGptProvider;
use euler_provider::custom_provider::CustomOpenAiProvider;
use euler_provider::openai::OpenAiProvider;
use euler_provider::openrouter::OpenRouterProvider;
use euler_provider::provider_config::ProviderConfigRegistry;
use euler_provider::xai::XaiProvider;
use euler_provider::{EchoProvider, ModelProvider, ProviderSet};
use std::path::{Path, PathBuf};

use crate::auth_validation::{validate_provider_auth, StoredApiKeyAuth, StoredChatGptAuth};
use crate::model_preference::{ModelPreference, PreferenceLoad, ThemePreferenceLoad};
use crate::theme_catalog::ThemeChoice;
use crate::{fixture_script, model_catalog, model_preference, provider_config_runtime};

pub(crate) fn provider_for_id(
    provider_id: &str,
    auth_file: Option<PathBuf>,
    options: &ProviderOptions,
    custom_providers: &ProviderConfigRegistry,
) -> Result<Box<dyn ModelProvider>> {
    let provider_id = parse_provider_id(provider_id, custom_providers)?;
    if let Ok(provider_id) = parse_known_provider_id(&provider_id) {
        return Ok(match provider_id.as_str() {
            "fixture" => fixture_provider(options)?,
            "chatgpt" => match auth_file {
                Some(path) => {
                    reject_provider_options("chatgpt", options)?;
                    Box::new(ChatGptProvider::legacy_auth_file(path))
                }
                None => {
                    reject_provider_options("chatgpt", options)?;
                    Box::new(ChatGptProvider::stored_euler_auth(
                        StoredChatGptAuth::new_default(),
                    ))
                }
            },
            "anthropic" => {
                reject_provider_options("anthropic", options)?;
                Box::new(AnthropicProvider::with_api_key_auth(api_key_auth(
                    auth_file,
                )))
            }
            "openai" => {
                reject_provider_options("openai", options)?;
                Box::new(OpenAiProvider::with_api_key_auth(api_key_auth(auth_file)))
            }
            "openrouter" => {
                reject_provider_options("openrouter", options)?;
                Box::new(OpenRouterProvider::with_api_key_auth(api_key_auth(
                    auth_file,
                )))
            }
            "xai" => {
                reject_provider_options("xai", options)?;
                Box::new(XaiProvider::with_api_key_auth(api_key_auth(auth_file)))
            }
            other => return Err(anyhow!("provider `{other}` is missing CLI factory wiring")),
        });
    }
    if let Some(provider) = custom_providers.provider(&provider_id) {
        reject_provider_options(&provider_id, options)?;
        if auth_file.is_some() {
            return Err(anyhow!(
                "--auth-file is not supported with provider {provider_id}"
            ));
        }
        return Ok(Box::new(CustomOpenAiProvider::from_config(
            provider.clone(),
        )?));
    }
    Err(anyhow!("unknown provider: {provider_id}"))
}

fn api_key_auth(auth_file: Option<PathBuf>) -> StoredApiKeyAuth {
    auth_file
        .map(StoredApiKeyAuth::auth_file)
        .unwrap_or_else(StoredApiKeyAuth::new_default)
}

fn fixture_provider(options: &ProviderOptions) -> Result<Box<dyn ModelProvider>> {
    if let Some(key) = options.keys().find(|key| *key != "event-script") {
        return Err(anyhow!(
            "provider option `{key}` is not supported by provider fixture"
        ));
    }
    match options.values.get("event-script").map(String::as_str) {
        None => Ok(Box::new(EchoProvider)),
        Some("") => Err(anyhow!("provider option `event-script` requires a value")),
        Some(path) => Ok(Box::new(fixture_script::provider_from_event_script_path(
            path,
        )?)),
    }
}

fn reject_provider_options(provider: &str, options: &ProviderOptions) -> Result<()> {
    if let Some(key) = options.keys().next() {
        Err(anyhow!(
            "provider option `{key}` is not supported by provider {provider}"
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn resume_provider_set(
    original: &ModelTarget,
    active: &ModelTarget,
    auth_file: Option<PathBuf>,
) -> Result<ProviderSet> {
    resume_provider_set_with_custom(
        original,
        active,
        auth_file,
        &ProviderConfigRegistry::default(),
    )
}

pub(crate) fn resume_provider_set_with_custom(
    original: &ModelTarget,
    active: &ModelTarget,
    auth_file: Option<PathBuf>,
    custom_providers: &ProviderConfigRegistry,
) -> Result<ProviderSet> {
    let mut providers = ProviderSet::new();
    // Only require auth for the active target — the provider that will
    // actually make API calls. The original target is historical context
    // for fold seeding; requiring its auth would break valid sessions
    // that switched away from a provider the user no longer has creds for.
    let active_provider = provider_for_id(
        &active.provider,
        auth_file.clone(),
        &ProviderOptions::default(),
        custom_providers,
    )?;
    validate_provider_auth(&active.provider, auth_file.as_deref(), || {
        active_provider.validate_auth()
    })?;
    providers.insert_named(active.provider.clone(), active_provider);
    if original.provider != active.provider {
        // Best-effort: insert original provider without auth requirement
        // so fold history is representable, but don't fail if unavailable.
        if let Ok(original_provider) = provider_for_id(
            &original.provider,
            auth_file,
            &ProviderOptions::default(),
            custom_providers,
        ) {
            let _ = original_provider.validate_auth(); // warn-worthy but not fatal
            providers.insert_named(original.provider.clone(), original_provider);
        }
    }
    // A resumed session must be able to /model-switch to any configured
    // provider, exactly like a fresh TUI session (review v2 §14.5 — switches
    // were rejected with "provider is not configured"). Auth stays lazy:
    // invoking an un-credentialed provider still fails loudly at call time.
    fill_provider_set(&mut providers, custom_providers);
    Ok(providers)
}

/// Best-effort: add every builtin + custom provider not already present.
fn fill_provider_set(providers: &mut ProviderSet, custom_providers: &ProviderConfigRegistry) {
    for descriptor in BUILTIN_PROVIDERS {
        insert_provider_if_missing(providers, descriptor.id, custom_providers);
    }
    let mut custom_ids = custom_providers
        .providers()
        .map(|provider| provider.id.as_str())
        .collect::<Vec<_>>();
    custom_ids.sort_unstable();
    for provider_id in custom_ids {
        insert_provider_if_missing(providers, provider_id, custom_providers);
    }
}

fn insert_provider_if_missing(
    providers: &mut ProviderSet,
    provider_id: &str,
    custom_providers: &ProviderConfigRegistry,
) {
    if providers.contains(provider_id) {
        return;
    }
    let Ok(provider) = provider_for_id(
        provider_id,
        None,
        &ProviderOptions::default(),
        custom_providers,
    ) else {
        return;
    };
    providers.insert_named(provider_id.to_owned(), provider);
}

pub(crate) fn tui_provider_set(
    active_provider_id: String,
    active_provider: Box<dyn ModelProvider>,
    custom_providers: &ProviderConfigRegistry,
) -> ProviderSet {
    let mut providers = ProviderSet::new();
    providers.insert_named(active_provider_id, active_provider);
    fill_provider_set(&mut providers, custom_providers);
    providers
}

pub(super) fn invocation_target(run: &RunArgs) -> ModelTarget {
    ModelTarget::new(run.provider_id.clone(), run.model.clone())
}

pub(super) fn load_known_model_preference(
    preference_path: Option<&Path>,
) -> Option<ModelPreference> {
    let path = preference_path?;
    match model_preference::load_model_preference(path) {
        PreferenceLoad::Loaded(preference) => canonical_model_preference(preference),
        PreferenceLoad::Missing => None,
        PreferenceLoad::Ignored(message) => {
            eprintln!("warning: ignored model preference: {message}");
            None
        }
    }
}

pub(crate) fn load_known_theme_preference(preference_path: Option<&Path>) -> Option<ThemeChoice> {
    let path = preference_path?;
    match model_preference::load_theme_preference(path) {
        ThemePreferenceLoad::Loaded(theme) => ThemeChoice::parse(&theme),
        ThemePreferenceLoad::Missing => None,
        ThemePreferenceLoad::Ignored(message) => {
            eprintln!("warning: ignored theme preference: {message}");
            None
        }
    }
}

pub(super) fn load_timestamps_preference(preference_path: Option<&Path>) -> Option<bool> {
    let path = preference_path?;
    match model_preference::load_timestamps_preference(path) {
        model_preference::TimestampsPreferenceLoad::Loaded(show) => Some(show),
        model_preference::TimestampsPreferenceLoad::Missing => None,
        model_preference::TimestampsPreferenceLoad::Ignored(message) => {
            eprintln!("warning: ignored timestamps preference: {message}");
            None
        }
    }
}

pub(super) fn load_notifications_preference(preference_path: Option<&Path>) -> Option<bool> {
    let path = preference_path?;
    match model_preference::load_notifications_preference(path) {
        model_preference::NotificationsPreferenceLoad::Loaded(enabled) => Some(enabled),
        model_preference::NotificationsPreferenceLoad::Missing => None,
        model_preference::NotificationsPreferenceLoad::Ignored(message) => {
            eprintln!("warning: ignored notifications preference: {message}");
            None
        }
    }
}

pub(super) fn load_known_model_catalog(model_catalog_path: Option<&Path>) -> MergedModelCatalog {
    let load = model_catalog::load_model_catalog(model_catalog_path);
    for warning in load.warnings {
        eprintln!("warning: ignored model catalog: {warning}");
    }
    load.catalog
}

pub(crate) fn load_custom_provider_config(
    provider_config_path: Option<&Path>,
) -> ProviderConfigRegistry {
    let load = provider_config_runtime::load_provider_config(provider_config_path);
    for warning in load.warnings {
        eprintln!("warning: ignored provider config: {warning}");
    }
    load.registry
}
