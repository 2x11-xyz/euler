use anyhow::{anyhow, Result};
use euler_provider::catalog::{
    auth_file_supported_by_provider, canonical_provider_id, MergedModelCatalog, FIXTURE_PROVIDER_ID,
};
use euler_provider::provider_config::ProviderConfigRegistry;
use std::path::PathBuf;

use crate::{model_preference::ModelPreference, ProviderOptions, RawArgs};

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct LiveOptions {
    pub(crate) provider_id: String,
    pub(crate) model: Option<String>,
    pub(crate) auth_file: Option<PathBuf>,
    pub(crate) provider_options: ProviderOptions,
}

pub(crate) fn resolve_live_options(
    parsed: &RawArgs,
    preference: Option<&ModelPreference>,
    custom_providers: &ProviderConfigRegistry,
) -> Result<LiveOptions> {
    let model_spec = parsed
        .model
        .as_deref()
        .map(|model| parse_model_spec(model, custom_providers))
        .transpose()?;
    let provider_id =
        resolved_provider_id(parsed, preference, model_spec.as_ref(), custom_providers)?;
    let model = resolved_model(preference, &provider_id, model_spec);

    if parsed.auth_file_from_cli && !auth_file_supported_by_provider(&provider_id) {
        return Err(anyhow!(
            "--auth-file is only supported with --provider chatgpt, anthropic, openai, openrouter, or xai"
        ));
    }

    let auth_file = if auth_file_supported_by_provider(&provider_id) {
        parsed.auth_file.clone()
    } else {
        None
    };

    Ok(LiveOptions {
        model,
        provider_id,
        auth_file,
        provider_options: parsed.provider_options.clone(),
    })
}

pub(crate) fn canonical_model_preference(
    mut preference: ModelPreference,
) -> Option<ModelPreference> {
    match canonical_provider_id(&preference.provider) {
        Some(provider) => {
            preference.provider = provider.to_owned();
            Some(preference)
        }
        None => {
            eprintln!(
                "warning: ignored model preference with unknown provider {}",
                preference.provider
            );
            None
        }
    }
}

pub(crate) fn parse_known_provider_id(provider: &str) -> Result<String> {
    let normalized = normalized_provider_id(provider)?;
    canonical_provider_id(&normalized)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("unknown provider: {normalized}"))
}

pub(crate) fn parse_provider_id(
    provider: &str,
    custom_providers: &ProviderConfigRegistry,
) -> Result<String> {
    let normalized = normalized_provider_id(provider)?;
    if let Some(provider) = canonical_provider_id(&normalized) {
        return Ok(provider.to_owned());
    }
    if custom_providers.provider(&normalized).is_some() {
        return Ok(normalized);
    }
    Err(anyhow!("unknown provider: {normalized}"))
}

pub(crate) fn default_model_for_provider(
    provider: &str,
    catalog: &MergedModelCatalog,
    custom_providers: &ProviderConfigRegistry,
) -> Result<String> {
    if let Some(model) = catalog.default_model_for_provider(provider) {
        return Ok(model.to_owned());
    }
    if let Some(provider) = custom_providers.provider(provider) {
        if let Some(model) = &provider.default_model {
            return Ok(model.clone());
        }
        if let Some(error) = &provider.default_model_error {
            return Err(anyhow!("{error}"));
        }
        let provider_id = &provider.id;
        return Err(anyhow!(
            "provider `{provider_id}` has no default_model; pass --model"
        ));
    }
    Err(anyhow!("unknown provider: {provider}"))
}

fn resolved_provider_id(
    parsed: &RawArgs,
    preference: Option<&ModelPreference>,
    model_spec: Option<&ModelSpec>,
    custom_providers: &ProviderConfigRegistry,
) -> Result<String> {
    if parsed.provider_from_cli {
        let provider_id = raw_provider_id(parsed, custom_providers)?;
        ensure_routed_model_matches_provider(
            &provider_id,
            model_spec,
            "--provider",
            model_source_label(parsed),
        )?;
        return Ok(provider_id);
    }

    if parsed.model_from_cli {
        if let Some(ModelSpec::Routed { provider, .. }) = model_spec {
            return Ok(provider.clone());
        }
    }

    if let Some(provider) = parsed.provider.as_deref() {
        let provider_id = parse_provider_id(provider, custom_providers)?;
        if !parsed.model_from_cli {
            ensure_routed_model_matches_provider(
                &provider_id,
                model_spec,
                "EULER_PROVIDER",
                model_source_label(parsed),
            )?;
        }
        return Ok(provider_id);
    }

    if let Some(ModelSpec::Routed { provider, .. }) = model_spec {
        return Ok(provider.clone());
    }

    Ok(preference
        .map(|preference| preference.provider.clone())
        .unwrap_or_else(|| FIXTURE_PROVIDER_ID.to_owned()))
}

pub(crate) fn raw_known_provider_id(parsed: &RawArgs) -> Result<String> {
    parsed
        .provider
        .as_deref()
        .ok_or_else(|| anyhow!("missing provider"))
        .and_then(parse_known_provider_id)
}

pub(crate) fn raw_provider_id(
    parsed: &RawArgs,
    custom_providers: &ProviderConfigRegistry,
) -> Result<String> {
    parsed
        .provider
        .as_deref()
        .ok_or_else(|| anyhow!("missing provider"))
        .and_then(|provider| parse_provider_id(provider, custom_providers))
}

fn ensure_routed_model_matches_provider(
    provider_id: &str,
    model_spec: Option<&ModelSpec>,
    provider_source: &str,
    model_source: &str,
) -> Result<()> {
    let Some(ModelSpec::Routed { provider, .. }) = model_spec else {
        return Ok(());
    };
    if provider == provider_id {
        return Ok(());
    }
    Err(anyhow!(
        "{provider_source} provider `{provider_id}` conflicts with route provider `{provider}` in {model_source}"
    ))
}

fn model_source_label(parsed: &RawArgs) -> &'static str {
    if parsed.model_from_cli {
        "--model"
    } else {
        "EULER_MODEL"
    }
}

fn resolved_model(
    preference: Option<&ModelPreference>,
    provider_id: &str,
    model_spec: Option<ModelSpec>,
) -> Option<String> {
    match model_spec {
        Some(ModelSpec::Plain(model)) => Some(model),
        Some(ModelSpec::Routed { model, .. }) => Some(model),
        None => preference
            .filter(|preference| preference.provider == provider_id)
            .map(|preference| preference.model.clone()),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ModelSpec {
    Plain(String),
    Routed { provider: String, model: String },
}

fn parse_model_spec(input: &str, custom_providers: &ProviderConfigRegistry) -> Result<ModelSpec> {
    if input.trim().is_empty() {
        return Err(anyhow!("model id is empty"));
    }
    let Some((provider, model)) = input.split_once("::") else {
        return Ok(ModelSpec::Plain(input.to_owned()));
    };
    if provider.trim().is_empty() {
        return Err(anyhow!("model route provider is empty"));
    }
    if model.trim().is_empty() {
        return Err(anyhow!("model route model is empty"));
    }
    Ok(ModelSpec::Routed {
        provider: parse_provider_id(provider, custom_providers)?,
        model: model.to_owned(),
    })
}

fn normalized_provider_id(provider: &str) -> Result<String> {
    let normalized = provider.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        Err(anyhow!("provider id is empty"))
    } else {
        Ok(normalized)
    }
}
