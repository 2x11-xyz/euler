//! Built-in provider metadata and runtime model-route parsing.

use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

pub const FIXTURE_PROVIDER_ID: &str = "fixture";
pub const CHATGPT_PROVIDER_ID: &str = "chatgpt";
pub const OPENAI_PROVIDER_ID: &str = "openai";
pub const ANTHROPIC_PROVIDER_ID: &str = "anthropic";
pub const OPENROUTER_PROVIDER_ID: &str = "openrouter";

pub const DEFAULT_FIXTURE_MODEL: &str = "echo";
pub const DEFAULT_CHATGPT_MODEL: &str = "gpt-5.5";
pub const DEFAULT_OPENAI_MODEL: &str = crate::openai::DEFAULT_MODEL;
pub const DEFAULT_ANTHROPIC_MODEL: &str = crate::anthropic::DEFAULT_MODEL;
pub const DEFAULT_OPENROUTER_MODEL: &str = crate::openrouter::DEFAULT_MODEL;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltInModelDescriptor {
    pub id: &'static str,
    pub display_name: &'static str,
    pub context_window_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub supports_tools: Option<bool>,
    pub supports_reasoning: Option<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderDescriptor {
    pub id: &'static str,
    pub display_name: &'static str,
    pub default_model: &'static str,
    pub aliases: &'static [&'static str],
    pub auth_file_supported: bool,
    pub models: &'static [BuiltInModelDescriptor],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelDescriptorSource {
    BuiltIn,
    Local,
}

impl ModelDescriptorSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::Local => "local",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelDescriptor {
    id: String,
    display_name: String,
    source: ModelDescriptorSource,
    context_window_tokens: Option<u64>,
    max_output_tokens: Option<u64>,
    supports_tools: Option<bool>,
    supports_reasoning: Option<bool>,
}

impl ModelDescriptor {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn source(&self) -> ModelDescriptorSource {
        self.source
    }

    pub fn context_window_tokens(&self) -> Option<u64> {
        self.context_window_tokens
    }

    pub fn max_output_tokens(&self) -> Option<u64> {
        self.max_output_tokens
    }

    pub fn supports_tools(&self) -> Option<bool> {
        self.supports_tools
    }

    pub fn supports_reasoning(&self) -> Option<bool> {
        self.supports_reasoning
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergedProviderDescriptor {
    id: &'static str,
    display_name: &'static str,
    default_model: String,
    auth_file_supported: bool,
    models: BTreeMap<String, ModelDescriptor>,
}

impl MergedProviderDescriptor {
    pub fn id(&self) -> &'static str {
        self.id
    }

    pub fn display_name(&self) -> &'static str {
        self.display_name
    }

    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    pub fn auth_file_supported(&self) -> bool {
        self.auth_file_supported
    }

    pub fn models(&self) -> impl Iterator<Item = &ModelDescriptor> {
        self.models.values()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergedModelCatalog {
    providers: BTreeMap<&'static str, MergedProviderDescriptor>,
}

pub const BUILTIN_PROVIDERS: &[ProviderDescriptor] = &[
    ProviderDescriptor {
        id: FIXTURE_PROVIDER_ID,
        display_name: "Fixture",
        default_model: DEFAULT_FIXTURE_MODEL,
        aliases: &["echo"],
        auth_file_supported: false,
        models: &[BuiltInModelDescriptor {
            id: DEFAULT_FIXTURE_MODEL,
            display_name: DEFAULT_FIXTURE_MODEL,
            context_window_tokens: None,
            max_output_tokens: None,
            supports_tools: None,
            supports_reasoning: None,
        }],
    },
    ProviderDescriptor {
        id: CHATGPT_PROVIDER_ID,
        display_name: "ChatGPT",
        default_model: DEFAULT_CHATGPT_MODEL,
        aliases: &[],
        auth_file_supported: true,
        models: OPENAI_FAMILY_MODELS,
    },
    ProviderDescriptor {
        id: OPENAI_PROVIDER_ID,
        display_name: "OpenAI",
        default_model: DEFAULT_OPENAI_MODEL,
        aliases: &[],
        auth_file_supported: true,
        models: OPENAI_FAMILY_MODELS,
    },
    ProviderDescriptor {
        id: ANTHROPIC_PROVIDER_ID,
        display_name: "Anthropic",
        default_model: DEFAULT_ANTHROPIC_MODEL,
        aliases: &[],
        auth_file_supported: true,
        models: ANTHROPIC_MODELS,
    },
    ProviderDescriptor {
        id: OPENROUTER_PROVIDER_ID,
        display_name: "OpenRouter",
        default_model: DEFAULT_OPENROUTER_MODEL,
        aliases: &[],
        auth_file_supported: true,
        models: OPENROUTER_MODELS,
    },
];

pub(crate) const ANTHROPIC_MODELS: &[BuiltInModelDescriptor] = &[
    built_in_model(
        "claude-sonnet-5",
        "Claude Sonnet 5",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model("claude-fable-5", "Claude Fable 5", 1_000_000, 128_000, true),
    built_in_model(
        "claude-opus-4-8",
        "Claude Opus 4.8",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "claude-sonnet-4-6",
        "Claude Sonnet 4.6",
        1_000_000,
        64_000,
        true,
    ),
    built_in_model(
        "claude-haiku-4-5",
        "Claude Haiku 4.5 (latest)",
        200_000,
        64_000,
        true,
    ),
];

const OPENAI_FAMILY_MODELS: &[BuiltInModelDescriptor] = &[
    built_in_model("gpt-5.5", "GPT-5.5", 1_050_000, 128_000, true),
    built_in_model("gpt-5.4", "GPT-5.4", 1_050_000, 128_000, true),
    built_in_model("gpt-5.4-mini", "GPT-5.4 mini", 400_000, 128_000, true),
    built_in_model("gpt-5.3-codex", "GPT-5.3 Codex", 400_000, 128_000, true),
];

const OPENROUTER_MODELS: &[BuiltInModelDescriptor] = &[
    built_in_model(
        "anthropic/claude-sonnet-5",
        "Claude Sonnet 5",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "anthropic/claude-fable-5",
        "Claude Fable 5",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model("openai/gpt-5.5", "GPT-5.5", 1_050_000, 128_000, true),
    built_in_model("openai/gpt-5.4", "GPT-5.4", 1_050_000, 128_000, true),
    built_in_model(
        "openai/gpt-4.1-mini",
        "GPT-4.1 mini",
        1_047_576,
        32_768,
        false,
    ),
    built_in_model("z-ai/glm-5.2", "GLM-5.2", 1_024_000, 128_000, true),
    built_in_model(
        "~google/gemini-pro-latest",
        "Google Gemini Pro Latest",
        1_048_576,
        65_536,
        true,
    ),
    built_in_model(
        "google/gemini-3.1-pro-preview",
        "Gemini 3.1 Pro Preview",
        1_048_576,
        65_536,
        true,
    ),
    built_in_model(
        "google/gemini-3.1-pro-preview-customtools",
        "Gemini 3.1 Pro Preview Custom Tools",
        1_048_576,
        65_536,
        true,
    ),
];

const fn built_in_model(
    id: &'static str,
    display_name: &'static str,
    context_window_tokens: u64,
    max_output_tokens: u64,
    supports_reasoning: bool,
) -> BuiltInModelDescriptor {
    BuiltInModelDescriptor {
        id,
        display_name,
        context_window_tokens: Some(context_window_tokens),
        max_output_tokens: Some(max_output_tokens),
        supports_tools: Some(true),
        supports_reasoning: Some(supports_reasoning),
    }
}

impl MergedModelCatalog {
    pub fn built_in() -> Self {
        let providers = BUILTIN_PROVIDERS
            .iter()
            .map(|descriptor| {
                let models = descriptor
                    .models
                    .iter()
                    .map(|model| {
                        (
                            model.id.to_owned(),
                            ModelDescriptor {
                                id: model.id.to_owned(),
                                display_name: model.display_name.to_owned(),
                                source: ModelDescriptorSource::BuiltIn,
                                context_window_tokens: model.context_window_tokens,
                                max_output_tokens: model.max_output_tokens,
                                supports_tools: model.supports_tools,
                                supports_reasoning: model.supports_reasoning,
                            },
                        )
                    })
                    .collect();
                (
                    descriptor.id,
                    MergedProviderDescriptor {
                        id: descriptor.id,
                        display_name: descriptor.display_name,
                        default_model: descriptor.default_model.to_owned(),
                        auth_file_supported: descriptor.auth_file_supported,
                        models,
                    },
                )
            })
            .collect();
        Self { providers }
    }

    pub fn with_local_json(contents: &str) -> (Self, Vec<String>) {
        let mut catalog = Self::built_in();
        let mut warnings = Vec::new();
        let contents = contents.trim_start_matches('\u{feff}');
        if contents.trim().is_empty() {
            warnings.push("models.json is empty; using built-in model catalog".to_owned());
            return (catalog, warnings);
        }
        let value: Value = match serde_json::from_str(contents) {
            Ok(value) => value,
            Err(error) => {
                warnings.push(format!(
                    "models.json is not valid JSON ({error}); using built-in model catalog"
                ));
                return (catalog, warnings);
            }
        };
        let Some(root) = value.as_object() else {
            warnings.push(
                "models.json root must be an object; using built-in model catalog".to_owned(),
            );
            return (catalog, warnings);
        };

        warn_unknown_fields(
            root.keys(),
            &["version", "generated_by", "providers"],
            "root",
            &mut warnings,
        );
        if !valid_config_version(root.get("version"), &mut warnings) {
            return (catalog, warnings);
        }

        let Some(providers) = root.get("providers") else {
            return (catalog, warnings);
        };
        let Some(providers) = providers.as_object() else {
            warnings.push(
                "models.json providers must be an object; using built-in model catalog".to_owned(),
            );
            return (catalog, warnings);
        };

        for (provider_key, provider_value) in providers {
            catalog.merge_provider(provider_key, provider_value, &mut warnings);
        }
        (catalog, warnings)
    }

    pub fn providers(&self) -> impl Iterator<Item = &MergedProviderDescriptor> {
        self.providers.values()
    }

    pub fn provider(&self, input: &str) -> Option<&MergedProviderDescriptor> {
        let id = canonical_provider_id(input)?;
        self.providers.get(id)
    }

    pub fn default_model_for_provider(&self, input: &str) -> Option<&str> {
        self.provider(input)
            .map(MergedProviderDescriptor::default_model)
    }

    fn merge_provider(
        &mut self,
        provider_key: &str,
        provider_value: &Value,
        warnings: &mut Vec<String>,
    ) {
        let Some(canonical_id) = canonical_provider_id(provider_key) else {
            warnings.push(format!(
                "ignored unknown provider `{provider_key}` in models.json"
            ));
            return;
        };
        if provider_key != canonical_id {
            warnings.push(format!(
                "ignored non-canonical provider key `{provider_key}` in models.json; use `{canonical_id}`"
            ));
            return;
        }
        let Some(provider) = self.providers.get_mut(canonical_id) else {
            warnings.push(format!(
                "ignored provider `{provider_key}` without built-in adapter wiring"
            ));
            return;
        };
        let Some(object) = provider_value.as_object() else {
            warnings.push(format!(
                "ignored provider `{provider_key}` in models.json because it is not an object"
            ));
            return;
        };

        warn_unknown_fields(
            object.keys(),
            &["default_model", "models"],
            &format!("provider `{provider_key}`"),
            warnings,
        );
        if let Some(default_model) = object.get("default_model") {
            match default_model.as_str() {
                Some(model) => {
                    if let Some(model) = valid_local_model_id(
                        model,
                        &format!("provider `{provider_key}` default_model"),
                        warnings,
                    ) {
                        provider.default_model = model;
                    }
                }
                None => warnings.push(format!(
                    "ignored provider `{provider_key}` default_model because it is not a string"
                )),
            }
        }
        if let Some(models) = object.get("models") {
            merge_models(provider, provider_key, models, warnings);
        }
    }
}

fn valid_config_version(version: Option<&Value>, warnings: &mut Vec<String>) -> bool {
    let Some(version) = version else {
        return true;
    };
    if version.as_u64() == Some(1) {
        true
    } else {
        warnings.push("ignored models.json because version is not 1".to_owned());
        false
    }
}

fn merge_models(
    provider: &mut MergedProviderDescriptor,
    provider_key: &str,
    models: &Value,
    warnings: &mut Vec<String>,
) {
    let Some(models) = models.as_array() else {
        warnings.push(format!(
            "ignored provider `{provider_key}` models because it is not an array"
        ));
        return;
    };
    for (index, model) in models.iter().enumerate() {
        let scope = format!("provider `{provider_key}` model #{index}");
        let Some(object) = model.as_object() else {
            warnings.push(format!("ignored {scope} because it is not an object"));
            continue;
        };
        warn_unknown_fields(
            object.keys(),
            &[
                "id",
                "display_name",
                "context_window_tokens",
                "max_output_tokens",
                "supports_tools",
                "supports_reasoning",
            ],
            &scope,
            warnings,
        );
        let Some(id) = object.get("id").and_then(Value::as_str) else {
            warnings.push(format!(
                "ignored {scope} because id is missing or not a string"
            ));
            continue;
        };
        let Some(id) = valid_local_model_id(id, &scope, warnings) else {
            continue;
        };
        let display_name = match object.get("display_name") {
            Some(value) => value.as_str().unwrap_or_else(|| {
                warnings.push(format!(
                    "ignored {scope} display_name because it is not a string"
                ));
                id.as_str()
            }),
            None => id.as_str(),
        }
        .to_owned();
        let context_window_tokens =
            optional_positive_u64(object, "context_window_tokens", &scope, warnings);
        let max_output_tokens =
            optional_positive_u64(object, "max_output_tokens", &scope, warnings);
        let supports_tools = optional_bool(object, "supports_tools", &scope, warnings);
        let supports_reasoning = optional_bool(object, "supports_reasoning", &scope, warnings);
        if provider
            .models
            .get(&id)
            .is_some_and(|model| model.source == ModelDescriptorSource::Local)
        {
            warnings.push(format!(
                "provider `{provider_key}` model `{id}` appeared more than once; last valid descriptor wins"
            ));
        }
        // Same-id local descriptors are listing metadata overrides only; the
        // adapter still owns runtime model acceptance and request shaping.
        provider.models.insert(
            id.clone(),
            ModelDescriptor {
                id,
                display_name,
                source: ModelDescriptorSource::Local,
                context_window_tokens,
                max_output_tokens,
                supports_tools,
                supports_reasoning,
            },
        );
    }
}

fn optional_positive_u64(
    object: &serde_json::Map<String, Value>,
    field: &str,
    scope: &str,
    warnings: &mut Vec<String>,
) -> Option<u64> {
    let value = object.get(field)?;
    let Some(number) = value.as_u64() else {
        warnings.push(format!(
            "ignored {scope} {field} because it must be a positive JSON integer"
        ));
        return None;
    };
    if number == 0 {
        warnings.push(format!(
            "ignored {scope} {field} because it must be greater than zero"
        ));
        return None;
    }
    Some(number)
}

fn optional_bool(
    object: &serde_json::Map<String, Value>,
    field: &str,
    scope: &str,
    warnings: &mut Vec<String>,
) -> Option<bool> {
    let value = object.get(field)?;
    match value.as_bool() {
        Some(value) => Some(value),
        None => {
            warnings.push(format!(
                "ignored {scope} {field} because it must be a boolean"
            ));
            None
        }
    }
}

fn valid_local_model_id(id: &str, scope: &str, warnings: &mut Vec<String>) -> Option<String> {
    if id.is_empty() {
        warnings.push(format!("ignored {scope} because model id is empty"));
        return None;
    }
    if id.trim() != id {
        warnings.push(format!(
            "ignored {scope} because model id has leading or trailing whitespace"
        ));
        return None;
    }
    if id.contains("::") {
        warnings.push(format!(
            "ignored {scope} because model id contains reserved route separator `::`"
        ));
        return None;
    }
    Some(id.to_owned())
}

fn warn_unknown_fields<'a>(
    fields: impl Iterator<Item = &'a String>,
    allowed: &[&str],
    scope: &str,
    warnings: &mut Vec<String>,
) {
    for field in fields {
        if allowed.contains(&field.as_str()) {
            continue;
        }
        if forbidden_metadata_field(field) {
            warnings.push(format!(
                "ignored forbidden {scope} field `{field}` in models.json"
            ));
        } else {
            warnings.push(format!(
                "ignored unknown {scope} field `{field}` in models.json"
            ));
        }
    }
}

fn forbidden_metadata_field(field: &str) -> bool {
    let field = field.to_ascii_lowercase();
    [
        "key", "secret", "token", "password", "auth", "header", "base_url", "baseurl",
    ]
    .iter()
    .any(|needle| field.contains(needle))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderId {
    id: &'static str,
}

impl ProviderId {
    pub fn parse(input: &str) -> Result<Self, CatalogError> {
        provider_descriptor(input).map(|descriptor| Self { id: descriptor.id })
    }

    pub fn as_str(self) -> &'static str {
        self.id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRoute {
    provider: ProviderId,
    model: String,
}

impl ModelRoute {
    pub fn provider(&self) -> ProviderId {
        self.provider
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn into_model(self) -> String {
        self.model
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelSpec {
    Plain(String),
    Routed(ModelRoute),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogError {
    EmptyProvider,
    UnknownProvider(String),
    EmptyModel,
    EmptyRouteProvider,
    EmptyRouteModel,
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyProvider => f.write_str("provider id is empty"),
            Self::UnknownProvider(provider) => write!(f, "unknown provider: {provider}"),
            Self::EmptyModel => f.write_str("model id is empty"),
            Self::EmptyRouteProvider => f.write_str("model route provider is empty"),
            Self::EmptyRouteModel => f.write_str("model route model is empty"),
        }
    }
}

impl std::error::Error for CatalogError {}

pub fn provider_descriptor(input: &str) -> Result<&'static ProviderDescriptor, CatalogError> {
    let normalized = normalize_provider_id(input)?;
    BUILTIN_PROVIDERS
        .iter()
        .find(|descriptor| {
            descriptor.id == normalized || descriptor.aliases.contains(&normalized.as_str())
        })
        .ok_or(CatalogError::UnknownProvider(normalized))
}

pub fn canonical_provider_id(input: &str) -> Option<&'static str> {
    provider_descriptor(input)
        .map(|descriptor| descriptor.id)
        .ok()
}

pub fn default_model_for_provider(input: &str) -> Option<&'static str> {
    provider_descriptor(input)
        .map(|descriptor| descriptor.default_model)
        .ok()
}

pub fn auth_file_supported_by_provider(input: &str) -> bool {
    provider_descriptor(input)
        .map(|descriptor| descriptor.auth_file_supported)
        .unwrap_or(false)
}

pub fn built_in_model_supports_reasoning(provider: &str, model: &str) -> bool {
    provider_descriptor(provider)
        .ok()
        .and_then(|provider| {
            provider
                .models
                .iter()
                .find(|candidate| candidate.id == model)
        })
        .and_then(|model| model.supports_reasoning)
        .unwrap_or(false)
}

pub fn parse_model_spec(input: &str) -> Result<ModelSpec, CatalogError> {
    if input.trim().is_empty() {
        return Err(CatalogError::EmptyModel);
    }
    let Some((provider, model)) = input.split_once("::") else {
        return Ok(ModelSpec::Plain(input.to_owned()));
    };
    if provider.trim().is_empty() {
        return Err(CatalogError::EmptyRouteProvider);
    }
    if model.trim().is_empty() {
        return Err(CatalogError::EmptyRouteModel);
    }
    Ok(ModelSpec::Routed(ModelRoute {
        provider: ProviderId::parse(provider)?,
        model: model.to_owned(),
    }))
}

fn normalize_provider_id(input: &str) -> Result<String, CatalogError> {
    let normalized = input.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        Err(CatalogError::EmptyProvider)
    } else {
        Ok(normalized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[path = "catalog_extra_tests.rs"]
    mod catalog_extra_tests;

    #[test]
    fn built_in_catalog_pins_current_defaults_and_auth_file_support() {
        assert_eq!(
            BUILTIN_PROVIDERS
                .iter()
                .map(|provider| (
                    provider.id,
                    provider.default_model,
                    provider.auth_file_supported
                ))
                .collect::<Vec<_>>(),
            vec![
                (FIXTURE_PROVIDER_ID, DEFAULT_FIXTURE_MODEL, false),
                (CHATGPT_PROVIDER_ID, DEFAULT_CHATGPT_MODEL, true),
                (OPENAI_PROVIDER_ID, DEFAULT_OPENAI_MODEL, true),
                (ANTHROPIC_PROVIDER_ID, DEFAULT_ANTHROPIC_MODEL, true),
                (OPENROUTER_PROVIDER_ID, DEFAULT_OPENROUTER_MODEL, true),
            ]
        );
    }

    #[test]
    fn provider_lookup_normalizes_case_and_aliases() {
        assert_eq!(
            canonical_provider_id(" OpenRouter "),
            Some(OPENROUTER_PROVIDER_ID)
        );
        assert_eq!(
            canonical_provider_id("ANTHROPIC"),
            Some(ANTHROPIC_PROVIDER_ID)
        );
        assert_eq!(canonical_provider_id("openai"), Some(OPENAI_PROVIDER_ID));
        assert_eq!(canonical_provider_id("echo"), Some(FIXTURE_PROVIDER_ID));
        assert_eq!(canonical_provider_id("missing"), None);
    }

    #[test]
    fn default_and_auth_file_lookup_use_alias_normalization() {
        assert_eq!(
            default_model_for_provider("echo"),
            Some(DEFAULT_FIXTURE_MODEL)
        );
        assert_eq!(
            default_model_for_provider("OpenRouter"),
            Some(DEFAULT_OPENROUTER_MODEL)
        );
        assert_eq!(
            default_model_for_provider("openai"),
            Some(DEFAULT_OPENAI_MODEL)
        );
        assert!(!auth_file_supported_by_provider("echo"));
        assert!(auth_file_supported_by_provider("openai"));
        assert!(auth_file_supported_by_provider("CHATGPT"));
    }

    #[test]
    fn routed_model_parses_provider_and_preserves_model_suffix() {
        assert_eq!(
            parse_model_spec("anthropic::claude-sonnet-4-6").expect("route"),
            ModelSpec::Routed(ModelRoute {
                provider: ProviderId {
                    id: ANTHROPIC_PROVIDER_ID
                },
                model: "claude-sonnet-4-6".to_owned(),
            })
        );
        assert_eq!(
            parse_model_spec("openrouter::anthropic/claude-sonnet-4-6").expect("route"),
            ModelSpec::Routed(ModelRoute {
                provider: ProviderId {
                    id: OPENROUTER_PROVIDER_ID
                },
                model: "anthropic/claude-sonnet-4-6".to_owned(),
            })
        );
        assert_eq!(
            parse_model_spec("OpenAI::gpt-4.1").expect("route"),
            ModelSpec::Routed(ModelRoute {
                provider: ProviderId {
                    id: OPENAI_PROVIDER_ID
                },
                model: "gpt-4.1".to_owned(),
            })
        );
        assert_eq!(
            parse_model_spec("ANTHROPIC::Custom::Model").expect("route"),
            ModelSpec::Routed(ModelRoute {
                provider: ProviderId {
                    id: ANTHROPIC_PROVIDER_ID
                },
                model: "Custom::Model".to_owned(),
            })
        );
        assert_eq!(
            parse_model_spec(" anthropic :: model ").expect("route"),
            ModelSpec::Routed(ModelRoute {
                provider: ProviderId {
                    id: ANTHROPIC_PROVIDER_ID
                },
                model: " model ".to_owned(),
            })
        );
    }

    #[test]
    fn routed_model_accepts_provider_aliases() {
        assert_eq!(
            parse_model_spec("echo::custom-model").expect("route"),
            ModelSpec::Routed(ModelRoute {
                provider: ProviderId {
                    id: FIXTURE_PROVIDER_ID
                },
                model: "custom-model".to_owned(),
            })
        );
    }

    #[test]
    fn plain_model_preserves_provider_scoped_id() {
        assert_eq!(
            parse_model_spec("openai/gpt-4.1-mini").expect("plain"),
            ModelSpec::Plain("openai/gpt-4.1-mini".to_owned())
        );
    }

    #[test]
    fn malformed_routes_fail_before_provider_construction() {
        assert_eq!(parse_model_spec(""), Err(CatalogError::EmptyModel));
        assert_eq!(parse_model_spec("   "), Err(CatalogError::EmptyModel));
        assert_eq!(
            parse_model_spec("::claude"),
            Err(CatalogError::EmptyRouteProvider)
        );
        assert_eq!(
            parse_model_spec("anthropic::"),
            Err(CatalogError::EmptyRouteModel)
        );
        assert_eq!(
            parse_model_spec("missing::model"),
            Err(CatalogError::UnknownProvider("missing".to_owned()))
        );
    }

    #[test]
    fn local_model_config_overrides_default_and_adds_descriptor() {
        let (catalog, warnings) = MergedModelCatalog::with_local_json(
            r#"{
              "version": 1,
              "providers": {
                "chatgpt": {
                  "default_model": "gpt-custom",
                  "models": [
                    { "id": "gpt-custom", "display_name": "GPT Custom" }
                  ]
                }
              }
            }"#,
        );

        assert!(warnings.is_empty(), "{warnings:?}");
        let chatgpt = catalog.provider("chatgpt").expect("chatgpt provider");
        assert_eq!(chatgpt.default_model(), "gpt-custom");
        let models = chatgpt.models().collect::<Vec<_>>();
        assert!(models
            .iter()
            .any(|model| model.id() == DEFAULT_CHATGPT_MODEL
                && model.source() == ModelDescriptorSource::BuiltIn));
        let local = models
            .iter()
            .find(|model| model.id() == "gpt-custom")
            .expect("local model");
        assert_eq!(local.display_name(), "GPT Custom");
        assert_eq!(local.source(), ModelDescriptorSource::Local);
    }

    #[test]
    fn local_default_does_not_require_descriptor() {
        let (catalog, warnings) = MergedModelCatalog::with_local_json(
            r#"{
              "version": 1,
              "providers": {
                "anthropic": {
                  "default_model": "claude-future"
                }
              }
            }"#,
        );

        assert!(warnings.is_empty(), "{warnings:?}");
        let anthropic = catalog.provider("anthropic").expect("anthropic provider");
        assert_eq!(anthropic.default_model(), "claude-future");
        assert!(!anthropic
            .models()
            .any(|model| model.id() == "claude-future"));
    }

    #[test]
    fn local_descriptor_replaces_built_in_listing_metadata_only() {
        let (catalog, warnings) = MergedModelCatalog::with_local_json(&format!(
            r#"{{
              "version": 1,
              "providers": {{
                "openrouter": {{
                  "models": [
                    {{ "id": "{DEFAULT_OPENROUTER_MODEL}", "display_name": "Custom Label" }}
                  ]
                }}
              }}
            }}"#
        ));

        assert!(warnings.is_empty(), "{warnings:?}");
        let openrouter = catalog.provider("openrouter").expect("openrouter");
        let model = openrouter
            .models()
            .find(|model| model.id() == DEFAULT_OPENROUTER_MODEL)
            .expect("default model");
        assert_eq!(model.display_name(), "Custom Label");
        assert_eq!(model.source(), ModelDescriptorSource::Local);
        assert_eq!(openrouter.default_model(), DEFAULT_OPENROUTER_MODEL);
    }
}
