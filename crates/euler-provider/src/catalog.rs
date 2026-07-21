//! Built-in provider metadata and runtime model-route parsing.

mod official;

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::OnceLock;

use crate::ReasoningEffort;

pub use official::{OfficialCatalogError, EMBEDDED_CATALOG_JSON, EMBEDDED_MANIFEST_JSON};

pub const FIXTURE_PROVIDER_ID: &str = "fixture";
pub const CHATGPT_PROVIDER_ID: &str = "chatgpt";
pub const OPENAI_PROVIDER_ID: &str = "openai";
pub const ANTHROPIC_PROVIDER_ID: &str = "anthropic";
pub const OPENROUTER_PROVIDER_ID: &str = "openrouter";
pub const XAI_PROVIDER_ID: &str = "xai";

pub const DEFAULT_FIXTURE_MODEL: &str = "echo";
pub const DEFAULT_CHATGPT_MODEL: &str = "gpt-5.5";
pub const DEFAULT_OPENAI_MODEL: &str = crate::openai::DEFAULT_MODEL;
pub const DEFAULT_ANTHROPIC_MODEL: &str = crate::anthropic::DEFAULT_MODEL;
pub const DEFAULT_OPENROUTER_MODEL: &str = crate::openrouter::DEFAULT_MODEL;
pub const DEFAULT_XAI_MODEL: &str = crate::xai::DEFAULT_MODEL;

/// Millionths of a USD per one million tokens. One rate unit is exactly one
/// pico-dollar per token, so a quote needs no floating-point arithmetic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelCostRates {
    pub input: u64,
    pub output: u64,
    pub cache_read: Option<u64>,
    pub cache_write_5m: Option<u64>,
    pub cache_write_1h: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelCostTier {
    pub input_tokens_above: u64,
    pub rates: ModelCostRates,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCost {
    pub rates: ModelCostRates,
    tiers: Vec<ModelCostTier>,
}

impl ModelCost {
    pub fn new(rates: ModelCostRates) -> Self {
        Self {
            rates,
            tiers: Vec::new(),
        }
    }

    pub fn with_tiers(rates: ModelCostRates, tiers: Vec<ModelCostTier>) -> Option<Self> {
        let mut previous = None;
        for tier in &tiers {
            if tier.input_tokens_above == 0
                || previous.is_some_and(|threshold| threshold >= tier.input_tokens_above)
            {
                return None;
            }
            previous = Some(tier.input_tokens_above);
        }
        Some(Self { rates, tiers })
    }

    pub fn tiers(&self) -> &[ModelCostTier] {
        &self.tiers
    }

    pub fn quote(&self, usage: &crate::Usage) -> Option<ModelUsageCost> {
        let uncached = usage.uncached_input_tokens?;
        let cache_read = usage.cached_tokens?;
        let cache_write_5m = usage.cache_write_5m_tokens?;
        let cache_write_1h = usage.cache_write_1h_tokens?;
        let input_total = uncached
            .checked_add(cache_read)?
            .checked_add(cache_write_5m)?
            .checked_add(cache_write_1h)?;
        if input_total != usage.input_tokens {
            return None;
        }
        let (tier_input_tokens_above, rates) = self.rates_for_input(input_total);
        let input_picos = uncached.checked_mul(rates.input)?;
        let output_picos = usage.output_tokens.checked_mul(rates.output)?;
        let cache_read_picos = component_cost(cache_read, rates.cache_read)?;
        let cache_write_5m_picos = component_cost(cache_write_5m, rates.cache_write_5m)?;
        let cache_write_1h_picos = component_cost(cache_write_1h, rates.cache_write_1h)?;
        let total_picos = input_picos
            .checked_add(output_picos)?
            .checked_add(cache_read_picos)?
            .checked_add(cache_write_5m_picos)?
            .checked_add(cache_write_1h_picos)?;
        Some(ModelUsageCost {
            input_picos,
            output_picos,
            cache_read_picos,
            cache_write_5m_picos,
            cache_write_1h_picos,
            total_picos,
            rates,
            tier_input_tokens_above,
        })
    }

    fn rates_for_input(&self, input_tokens: u64) -> (Option<u64>, ModelCostRates) {
        self.tiers
            .iter()
            .rev()
            .find(|tier| input_tokens > tier.input_tokens_above)
            .map_or((None, self.rates), |tier| {
                (Some(tier.input_tokens_above), tier.rates)
            })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelUsageCost {
    pub input_picos: u64,
    pub output_picos: u64,
    pub cache_read_picos: u64,
    pub cache_write_5m_picos: u64,
    pub cache_write_1h_picos: u64,
    pub total_picos: u64,
    pub rates: ModelCostRates,
    pub tier_input_tokens_above: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCostSource {
    Official { release_id: String },
    Local { cost_sha256: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedModelCost {
    pub provider: String,
    pub model: String,
    pub cost: ModelCost,
    pub source: ModelCostSource,
}

fn component_cost(tokens: u64, rate: Option<u64>) -> Option<u64> {
    if tokens == 0 {
        Some(0)
    } else {
        tokens.checked_mul(rate?)
    }
}

const STANDARD_REASONING_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::XSmall,
    ReasoningEffort::Small,
    ReasoningEffort::Medium,
    ReasoningEffort::Large,
    ReasoningEffort::XLarge,
];
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderDescriptor {
    pub id: &'static str,
    pub display_name: &'static str,
    pub default_model: &'static str,
    pub aliases: &'static [&'static str],
    pub auth_file_supported: bool,
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
    reasoning_efforts: Vec<ReasoningEffort>,
    official_route: bool,
    effective_context_window_percent: Option<u8>,
    auto_compact_token_limit: Option<u64>,
    cost: Option<ModelCost>,
    local_cost_sha256: Option<String>,
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

    pub fn effective_context_window_tokens(&self) -> Option<u64> {
        let raw = self.context_window_tokens?;
        Some(match self.effective_context_window_percent {
            Some(percent) => raw.saturating_mul(u64::from(percent)) / 100,
            None => raw,
        })
    }

    pub fn auto_compact_token_limit(&self) -> Option<u64> {
        self.auto_compact_token_limit
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

    pub fn reasoning_efforts(&self) -> &[ReasoningEffort] {
        &self.reasoning_efforts
    }

    pub fn cost(&self) -> Option<&ModelCost> {
        self.cost.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergedProviderDescriptor {
    id: &'static str,
    display_name: String,
    default_model: String,
    auth_file_supported: bool,
    models: BTreeMap<String, ModelDescriptor>,
}

impl MergedProviderDescriptor {
    pub fn id(&self) -> &'static str {
        self.id
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
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
    official_release_id: Option<String>,
}

pub const BUILTIN_PROVIDERS: &[ProviderDescriptor] = &[
    ProviderDescriptor {
        id: FIXTURE_PROVIDER_ID,
        display_name: "Fixture",
        default_model: DEFAULT_FIXTURE_MODEL,
        aliases: &["echo"],
        auth_file_supported: false,
    },
    ProviderDescriptor {
        id: CHATGPT_PROVIDER_ID,
        display_name: "ChatGPT",
        default_model: DEFAULT_CHATGPT_MODEL,
        aliases: &[],
        auth_file_supported: true,
    },
    ProviderDescriptor {
        id: OPENAI_PROVIDER_ID,
        display_name: "OpenAI",
        default_model: DEFAULT_OPENAI_MODEL,
        aliases: &[],
        auth_file_supported: true,
    },
    ProviderDescriptor {
        id: ANTHROPIC_PROVIDER_ID,
        display_name: "Anthropic",
        default_model: DEFAULT_ANTHROPIC_MODEL,
        aliases: &[],
        auth_file_supported: true,
    },
    ProviderDescriptor {
        id: OPENROUTER_PROVIDER_ID,
        display_name: "OpenRouter",
        default_model: DEFAULT_OPENROUTER_MODEL,
        aliases: &[],
        auth_file_supported: true,
    },
    ProviderDescriptor {
        id: XAI_PROVIDER_ID,
        display_name: "xAI",
        default_model: DEFAULT_XAI_MODEL,
        aliases: &[],
        auth_file_supported: true,
    },
];

const CHATGPT_EFFECTIVE_CONTEXT_WINDOW_PERCENT: u8 = 95;

const fn chatgpt_context_policy(context_window_tokens: Option<u64>) -> (Option<u8>, Option<u64>) {
    match context_window_tokens {
        Some(raw) => (
            Some(CHATGPT_EFFECTIVE_CONTEXT_WINDOW_PERCENT),
            Some(raw * 9 / 10),
        ),
        None => (None, None),
    }
}

impl MergedModelCatalog {
    pub fn built_in() -> Self {
        embedded_catalog().clone()
    }

    pub fn from_official_json(contents: &str) -> Result<Self, OfficialCatalogError> {
        let document = official::parse(contents)?;
        let mut providers = BTreeMap::new();
        providers.insert(FIXTURE_PROVIDER_ID, fixture_provider());
        for (provider_id, provider) in document.providers {
            let adapter = provider_descriptor(&provider_id)
                .map_err(|error| OfficialCatalogError::new(error.to_string()))?;
            let mut models = BTreeMap::new();
            for model in provider.models {
                if model.status == official::OfficialModelStatus::Removed {
                    continue;
                }
                let cost = model
                    .cost
                    .as_ref()
                    .map(|cost| parse_official_model_cost(cost, &provider_id, &model.id))
                    .transpose()?;
                let reasoning_efforts = model.reasoning_efforts();
                let (effective_context_window_percent, auto_compact_token_limit) =
                    if provider_id == CHATGPT_PROVIDER_ID {
                        chatgpt_context_policy(Some(model.context_window_tokens))
                    } else {
                        (None, None)
                    };
                models.insert(
                    model.id.clone(),
                    ModelDescriptor {
                        id: model.id,
                        display_name: model.display_name,
                        source: ModelDescriptorSource::BuiltIn,
                        context_window_tokens: Some(model.context_window_tokens),
                        max_output_tokens: model.max_output_tokens,
                        supports_tools: Some(model.supports_tools),
                        supports_reasoning: Some(model.supports_reasoning),
                        reasoning_efforts,
                        official_route: true,
                        effective_context_window_percent,
                        auto_compact_token_limit,
                        cost,
                        local_cost_sha256: None,
                    },
                );
            }
            for alias in provider.aliases {
                models.insert(
                    alias.clone(),
                    ModelDescriptor {
                        id: alias.clone(),
                        display_name: alias,
                        source: ModelDescriptorSource::BuiltIn,
                        context_window_tokens: None,
                        max_output_tokens: None,
                        supports_tools: Some(true),
                        supports_reasoning: None,
                        reasoning_efforts: Vec::new(),
                        official_route: true,
                        effective_context_window_percent: None,
                        auto_compact_token_limit: None,
                        cost: None,
                        local_cost_sha256: None,
                    },
                );
            }
            providers.insert(
                adapter.id,
                MergedProviderDescriptor {
                    id: adapter.id,
                    display_name: provider.display_name,
                    default_model: provider.default_model,
                    auth_file_supported: adapter.auth_file_supported,
                    models,
                },
            );
        }
        Ok(Self {
            providers,
            official_release_id: None,
        })
    }

    pub fn with_official_release_id(mut self, release_id: impl Into<String>) -> Self {
        self.official_release_id = Some(release_id.into());
        self
    }

    pub fn with_local_json(contents: &str) -> (Self, Vec<String>) {
        Self::with_base_and_local_json(Self::built_in(), contents)
    }

    pub fn with_base_and_local_json(mut catalog: Self, contents: &str) -> (Self, Vec<String>) {
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
            &["version", "providers"],
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

    pub fn model(&self, provider: &str, model: &str) -> Option<&ModelDescriptor> {
        self.provider(provider)?.models.get(model)
    }

    pub fn resolved_model_cost(&self, provider: &str, model: &str) -> Option<ResolvedModelCost> {
        let provider = canonical_provider_id(provider)?;
        let descriptor = self.model(provider, model)?;
        let cost = descriptor.cost.clone()?;
        let source = match descriptor.source {
            ModelDescriptorSource::BuiltIn => ModelCostSource::Official {
                release_id: self.official_release_id.clone()?,
            },
            ModelDescriptorSource::Local => ModelCostSource::Local {
                cost_sha256: descriptor.local_cost_sha256.clone()?,
            },
        };
        Some(ResolvedModelCost {
            provider: provider.to_owned(),
            model: descriptor.id.clone(),
            cost,
            source,
        })
    }

    pub fn supported_reasoning_efforts(&self, provider: &str, model: &str) -> &[ReasoningEffort] {
        self.model(provider, model)
            .map(ModelDescriptor::reasoning_efforts)
            .filter(|efforts| !efforts.is_empty())
            .unwrap_or(STANDARD_REASONING_EFFORTS)
    }

    pub fn clamp_reasoning_effort(
        &self,
        provider: &str,
        model: &str,
        requested: ReasoningEffort,
    ) -> ReasoningEffort {
        let Some(model) = self
            .model(provider, model)
            .filter(|model| model.official_route)
        else {
            return requested;
        };
        let supported = if model.reasoning_efforts().is_empty() {
            STANDARD_REASONING_EFFORTS
        } else {
            model.reasoning_efforts()
        };
        if supported.contains(&requested) {
            requested
        } else {
            supported
                .last()
                .copied()
                .expect("reasoning effort catalog must not be empty")
        }
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

fn embedded_catalog() -> &'static MergedModelCatalog {
    static CATALOG: OnceLock<MergedModelCatalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        MergedModelCatalog::from_official_json(EMBEDDED_CATALOG_JSON)
            .expect("packaged provider catalog must be valid")
            .with_official_release_id(official::embedded_release_id())
    })
}

fn fixture_provider() -> MergedProviderDescriptor {
    MergedProviderDescriptor {
        id: FIXTURE_PROVIDER_ID,
        display_name: "Fixture".to_owned(),
        default_model: DEFAULT_FIXTURE_MODEL.to_owned(),
        auth_file_supported: false,
        models: BTreeMap::from([(
            DEFAULT_FIXTURE_MODEL.to_owned(),
            ModelDescriptor {
                id: DEFAULT_FIXTURE_MODEL.to_owned(),
                display_name: DEFAULT_FIXTURE_MODEL.to_owned(),
                source: ModelDescriptorSource::BuiltIn,
                context_window_tokens: None,
                max_output_tokens: None,
                supports_tools: None,
                supports_reasoning: None,
                reasoning_efforts: Vec::new(),
                official_route: true,
                effective_context_window_percent: None,
                auto_compact_token_limit: None,
                cost: None,
                local_cost_sha256: None,
            },
        )]),
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
        let Some(descriptor) =
            local_model_descriptor(provider, provider_key, model, &scope, warnings)
        else {
            continue;
        };
        let id = descriptor.id.clone();
        if provider
            .models
            .get(&id)
            .is_some_and(|model| model.source == ModelDescriptorSource::Local)
        {
            warnings.push(format!(
                "provider `{provider_key}` model `{id}` appeared more than once; last valid descriptor wins"
            ));
        }
        provider.models.insert(id, descriptor);
    }
}

fn local_model_descriptor(
    provider: &MergedProviderDescriptor,
    provider_key: &str,
    model: &Value,
    scope: &str,
    warnings: &mut Vec<String>,
) -> Option<ModelDescriptor> {
    let Some(object) = model.as_object() else {
        warnings.push(format!("ignored {scope} because it is not an object"));
        return None;
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
            "cost",
        ],
        scope,
        warnings,
    );
    let Some(id) = object.get("id").and_then(Value::as_str) else {
        warnings.push(format!(
            "ignored {scope} because id is missing or not a string"
        ));
        return None;
    };
    let id = valid_local_model_id(id, scope, warnings)?;
    let context_window_tokens =
        optional_positive_u64(object, "context_window_tokens", scope, warnings);
    let (reasoning_efforts, official_route) = provider
        .models
        .get(&id)
        .map(|model| (model.reasoning_efforts.clone(), model.official_route))
        .unwrap_or_default();
    let (cost, local_cost_sha256) = parse_model_cost(object, scope, warnings)
        .map(|(cost, digest)| (Some(cost), Some(digest)))
        .unwrap_or_default();
    let (effective_context_window_percent, auto_compact_token_limit) =
        if provider_key == CHATGPT_PROVIDER_ID {
            chatgpt_context_policy(context_window_tokens)
        } else {
            (None, None)
        };
    Some(ModelDescriptor {
        display_name: local_display_name(object, &id, scope, warnings),
        max_output_tokens: optional_positive_u64(object, "max_output_tokens", scope, warnings),
        supports_tools: optional_bool(object, "supports_tools", scope, warnings),
        supports_reasoning: optional_bool(object, "supports_reasoning", scope, warnings),
        source: ModelDescriptorSource::Local,
        context_window_tokens,
        reasoning_efforts,
        official_route,
        effective_context_window_percent,
        auto_compact_token_limit,
        local_cost_sha256,
        cost,
        id,
    })
}

fn local_display_name(
    object: &serde_json::Map<String, Value>,
    id: &str,
    scope: &str,
    warnings: &mut Vec<String>,
) -> String {
    match object.get("display_name") {
        Some(value) => value.as_str().unwrap_or_else(|| {
            warnings.push(format!(
                "ignored {scope} display_name because it is not a string"
            ));
            id
        }),
        None => id,
    }
    .to_owned()
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

fn parse_model_cost(
    object: &serde_json::Map<String, Value>,
    scope: &str,
    warnings: &mut Vec<String>,
) -> Option<(ModelCost, String)> {
    let value = object.get("cost")?;
    match parse_cost_value(value) {
        Ok(cost) => {
            let digest = model_cost_sha256(&cost);
            Some((cost, digest))
        }
        Err(error) => {
            warnings.push(format!("ignored {scope} cost because {error}"));
            None
        }
    }
}

pub(super) fn parse_official_model_cost(
    value: &Value,
    provider: &str,
    model: &str,
) -> Result<ModelCost, OfficialCatalogError> {
    parse_cost_value(value).map_err(|error| {
        OfficialCatalogError::new(format!(
            "provider `{provider}` model `{model}` cost is invalid: {error}"
        ))
    })
}

fn parse_cost_value(value: &Value) -> Result<ModelCost, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "it must be an object".to_owned())?;
    reject_unknown_cost_fields(object, false)?;
    let rates = parse_cost_rates(object)?;
    let Some(value) = object.get("tiers") else {
        return Ok(ModelCost::new(rates));
    };
    let values = value
        .as_array()
        .filter(|tiers| tiers.len() <= 16)
        .ok_or_else(|| "tiers must be an array with at most 16 entries".to_owned())?;
    let mut tiers = Vec::with_capacity(values.len());
    for value in values {
        let tier = value
            .as_object()
            .ok_or_else(|| "every tier must be an object".to_owned())?;
        reject_unknown_cost_fields(tier, true)?;
        let input_tokens_above = tier
            .get("input_tokens_above")
            .and_then(Value::as_u64)
            .ok_or_else(|| "every tier needs an integer input_tokens_above".to_owned())?;
        tiers.push(ModelCostTier {
            input_tokens_above,
            rates: parse_cost_rates(tier)?,
        });
    }
    ModelCost::with_tiers(rates, tiers)
        .ok_or_else(|| "tier thresholds must be positive, unique, and ascending".to_owned())
}

fn reject_unknown_cost_fields(
    object: &serde_json::Map<String, Value>,
    tier: bool,
) -> Result<(), String> {
    const RATE_FIELDS: [&str; 5] = [
        "input",
        "output",
        "cache_read",
        "cache_write_5m",
        "cache_write_1h",
    ];
    for field in object.keys() {
        let allowed = RATE_FIELDS.contains(&field.as_str())
            || (!tier && field == "tiers")
            || (tier && field == "input_tokens_above");
        if !allowed {
            return Err(format!("unknown field `{field}`"));
        }
    }
    Ok(())
}

fn parse_cost_rates(object: &serde_json::Map<String, Value>) -> Result<ModelCostRates, String> {
    Ok(ModelCostRates {
        input: required_cost_rate(object, "input")?,
        output: required_cost_rate(object, "output")?,
        cache_read: optional_cost_rate(object, "cache_read")?,
        cache_write_5m: optional_cost_rate(object, "cache_write_5m")?,
        cache_write_1h: optional_cost_rate(object, "cache_write_1h")?,
    })
}

fn required_cost_rate(object: &serde_json::Map<String, Value>, field: &str) -> Result<u64, String> {
    let value = object
        .get(field)
        .ok_or_else(|| format!("{field} is required"))?;
    parse_cost_rate(value).map_err(|error| format!("{field} {error}"))
}

fn optional_cost_rate(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<u64>, String> {
    object
        .get(field)
        .map(parse_cost_rate)
        .transpose()
        .map_err(|error| format!("{field} {error}"))
}

fn parse_cost_rate(value: &Value) -> Result<u64, String> {
    const SCALE: u64 = 1_000_000;
    const MAX_RATE: u64 = 1_000_000 * SCALE;
    let text = value
        .as_number()
        .map(ToString::to_string)
        .ok_or_else(|| "must be a non-negative number".to_owned())?;
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    if whole.starts_with('-') || whole.is_empty() || fraction.len() > 6 {
        return Err("must be non-negative with at most six decimal places".to_owned());
    }
    let whole = whole
        .parse::<u64>()
        .map_err(|_| "is out of bounds".to_owned())?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        let digits = fraction
            .parse::<u64>()
            .map_err(|_| "must be an ordinary decimal".to_owned())?;
        digits
            .checked_mul(10_u64.pow(6 - fraction.len() as u32))
            .ok_or_else(|| "is out of bounds".to_owned())?
    };
    whole
        .checked_mul(SCALE)
        .and_then(|value| value.checked_add(fraction))
        .filter(|value| *value <= MAX_RATE)
        .ok_or_else(|| "is out of bounds".to_owned())
}

fn model_cost_sha256(cost: &ModelCost) -> String {
    let mut digest = Sha256::new();
    update_cost_digest(&mut digest, None, cost.rates);
    for tier in cost.tiers() {
        update_cost_digest(&mut digest, Some(tier.input_tokens_above), tier.rates);
    }
    format!("{:x}", digest.finalize())
}

fn update_cost_digest(digest: &mut Sha256, threshold: Option<u64>, rates: ModelCostRates) {
    for value in [
        threshold,
        Some(rates.input),
        Some(rates.output),
        rates.cache_read,
        rates.cache_write_5m,
        rates.cache_write_1h,
    ] {
        digest.update([u8::from(value.is_some())]);
        if let Some(value) = value {
            digest.update(value.to_be_bytes());
        }
    }
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
    embedded_catalog()
        .model(provider, model)
        .and_then(ModelDescriptor::supports_reasoning)
        .unwrap_or(false)
}

pub fn supported_reasoning_efforts(provider: &str, model: &str) -> &'static [ReasoningEffort] {
    embedded_catalog().supported_reasoning_efforts(provider, model)
}

pub fn model_supports_reasoning_effort(
    provider: &str,
    model: &str,
    effort: ReasoningEffort,
) -> bool {
    supported_reasoning_efforts(provider, model).contains(&effort)
}

/// Preserve a requested effort for targets outside the built-in catalog or
/// when the known target supports it. Otherwise degrade to the known target's
/// highest advertised level. Effort lists are ordered from least to most
/// intensive and are never empty.
pub fn clamp_reasoning_effort(
    provider: &str,
    model: &str,
    requested: ReasoningEffort,
) -> ReasoningEffort {
    embedded_catalog().clamp_reasoning_effort(provider, model, requested)
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
    fn gpt_5_6_reasoning_efforts_match_route_capabilities() {
        assert!(model_supports_reasoning_effort(
            CHATGPT_PROVIDER_ID,
            "gpt-5.6-luna",
            ReasoningEffort::Max
        ));
        for model in ["gpt-5.6-sol", "gpt-5.6-terra"] {
            assert!(model_supports_reasoning_effort(
                CHATGPT_PROVIDER_ID,
                model,
                ReasoningEffort::Max
            ));
        }
        assert!(!model_supports_reasoning_effort(
            CHATGPT_PROVIDER_ID,
            "gpt-5.5",
            ReasoningEffort::Max
        ));
        assert_eq!(
            clamp_reasoning_effort(CHATGPT_PROVIDER_ID, "gpt-5.6-sol", ReasoningEffort::Max),
            ReasoningEffort::Max
        );
        assert_eq!(
            clamp_reasoning_effort(CHATGPT_PROVIDER_ID, "gpt-5.5", ReasoningEffort::Max),
            ReasoningEffort::XLarge
        );
        assert_eq!(
            clamp_reasoning_effort("custom", "model", ReasoningEffort::Max),
            ReasoningEffort::Max
        );
    }

    #[test]
    fn openrouter_reasoning_efforts_match_route_capabilities() {
        assert_eq!(
            supported_reasoning_efforts(OPENROUTER_PROVIDER_ID, "moonshotai/kimi-k3"),
            &[
                ReasoningEffort::Small,
                ReasoningEffort::Large,
                ReasoningEffort::Max
            ]
        );
        assert_eq!(
            clamp_reasoning_effort(
                OPENROUTER_PROVIDER_ID,
                "moonshotai/kimi-k3",
                ReasoningEffort::Medium
            ),
            ReasoningEffort::Max
        );
        assert!(!model_supports_reasoning_effort(
            OPENROUTER_PROVIDER_ID,
            "thinkingmachines/inkling",
            ReasoningEffort::XLarge
        ));
        assert!(model_supports_reasoning_effort(
            OPENROUTER_PROVIDER_ID,
            "thinkingmachines/inkling",
            ReasoningEffort::Max
        ));
    }

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
                (XAI_PROVIDER_ID, DEFAULT_XAI_MODEL, true),
            ]
        );
    }

    #[test]
    fn xai_built_in_models_include_routable_default_and_aliases() {
        let catalog = MergedModelCatalog::built_in();
        let xai = catalog.provider(XAI_PROVIDER_ID).expect("xai provider");
        let default = xai
            .models()
            .find(|model| model.id() == DEFAULT_XAI_MODEL)
            .expect("default xai model listed");
        assert_eq!(default.display_name(), "Grok 4.3");
        assert_eq!(default.supports_reasoning(), Some(true));
        assert!(built_in_model_supports_reasoning("xai", DEFAULT_XAI_MODEL));
        assert!(xai.models().any(|model| model.id() == "grok-latest"));
        assert!(xai.models().any(|model| model.id() == "grok-code-fast-1"));

        assert_eq!(
            parse_model_spec("xai::grok-4.3").expect("route"),
            ModelSpec::Routed(ModelRoute {
                provider: ProviderId {
                    id: XAI_PROVIDER_ID
                },
                model: "grok-4.3".to_owned(),
            })
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

    #[test]
    fn local_listing_metadata_cannot_change_official_reasoning_policy() {
        let (catalog, warnings) = MergedModelCatalog::with_local_json(
            r#"{
              "version": 1,
              "providers": {
                "openrouter": {
                  "models": [
                    { "id": "moonshotai/kimi-k3", "display_name": "Local label" },
                    { "id": "future/local-model", "display_name": "Future model" }
                  ]
                }
              }
            }"#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");

        assert_eq!(
            catalog.clamp_reasoning_effort(
                OPENROUTER_PROVIDER_ID,
                "moonshotai/kimi-k3",
                ReasoningEffort::Medium,
            ),
            ReasoningEffort::Max
        );
        assert_eq!(
            catalog.clamp_reasoning_effort(
                OPENROUTER_PROVIDER_ID,
                "future/local-model",
                ReasoningEffort::Max,
            ),
            ReasoningEffort::Max
        );
    }

    #[test]
    fn local_pricing_quotes_disjoint_usage_with_exact_decimal_rates() {
        let (catalog, warnings) = MergedModelCatalog::with_local_json(
            r#"{
              "version": 1,
              "providers": {"openrouter": {"models": [{
                "id": "z-ai/glm-5.2",
                "cost": {
                  "input": 0.532,
                  "output": 1.672,
                  "cache_read": 0.0988,
                  "cache_write_5m": 0.665
                }
              }]}}
            }"#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let resolved = catalog
            .resolved_model_cost("openrouter", "z-ai/glm-5.2")
            .expect("local price");
        assert!(matches!(
            resolved.source,
            ModelCostSource::Local { ref cost_sha256 } if cost_sha256.len() == 64
        ));
        let usage = crate::Usage {
            input_tokens: 10,
            output_tokens: 3,
            uncached_input_tokens: Some(7),
            cached_tokens: Some(2),
            cache_write_5m_tokens: Some(1),
            cache_write_1h_tokens: Some(0),
            reasoning_tokens: None,
        };

        let quote = resolved.cost.quote(&usage).expect("quote");

        assert_eq!(quote.input_picos, 3_724_000);
        assert_eq!(quote.output_picos, 5_016_000);
        assert_eq!(quote.cache_read_picos, 197_600);
        assert_eq!(quote.cache_write_5m_picos, 665_000);
        assert_eq!(quote.total_picos, 9_602_600);
    }

    #[test]
    fn pricing_tiers_select_one_request_wide_schedule_above_each_threshold() {
        let cost = parse_cost_value(&serde_json::json!({
            "input": 1,
            "output": 1,
            "tiers": [
                {"input_tokens_above": 10, "input": 2, "output": 2},
                {"input_tokens_above": 20, "input": 3, "output": 3}
            ]
        }))
        .expect("cost");

        assert_eq!(quote_input(&cost, 10), 10_000_000);
        assert_eq!(quote_input(&cost, 11), 22_000_000);
        assert_eq!(quote_input(&cost, 20), 40_000_000);
        assert_eq!(quote_input(&cost, 21), 63_000_000);
    }

    #[test]
    fn malformed_local_pricing_disables_pricing_without_falling_back() {
        let (catalog, warnings) = MergedModelCatalog::with_local_json(
            r#"{
              "version": 1,
              "providers": {"openai": {"models": [{
                "id": "gpt-5.5",
                "cost": {
                  "input": 5,
                  "output": 30,
                  "tiers": [
                    {"input_tokens_above": 20, "input": 10, "output": 45},
                    {"input_tokens_above": 20, "input": 11, "output": 46}
                  ]
                }
              }]}}
            }"#,
        );

        assert!(warnings
            .iter()
            .any(|warning| warning
                .contains("tier thresholds must be positive, unique, and ascending")));
        assert!(catalog.resolved_model_cost("openai", "gpt-5.5").is_none());
    }

    #[test]
    fn pricing_rejects_unknown_fields_and_excess_precision() {
        for cost in [
            serde_json::json!({"input": 1, "output": 2, "surprise": 3}),
            serde_json::json!({"input": 0.1234567, "output": 2}),
        ] {
            assert!(parse_cost_value(&cost).is_err());
        }
    }

    #[test]
    fn quote_fails_closed_for_inconsistent_usage_missing_rates_and_overflow() {
        let cost = ModelCost::new(ModelCostRates {
            input: u64::MAX,
            output: 1,
            cache_read: None,
            cache_write_5m: None,
            cache_write_1h: None,
        });
        let inconsistent = priced_usage(3, 2, 0, 0, 0);
        let missing_cache_rate = priced_usage(3, 2, 1, 0, 0);
        let overflow = priced_usage(2, 2, 0, 0, 0);

        assert!(cost.quote(&inconsistent).is_none());
        assert!(cost.quote(&missing_cache_rate).is_none());
        assert!(cost.quote(&overflow).is_none());
    }

    #[test]
    fn quote_distinguishes_known_zero_from_unpriced() {
        let cost = ModelCost::new(ModelCostRates {
            input: 1,
            output: 1,
            cache_read: None,
            cache_write_5m: None,
            cache_write_1h: None,
        });

        let quote = cost
            .quote(&priced_usage(0, 0, 0, 0, 0))
            .expect("zero-token call is priced");

        assert_eq!(quote.total_picos, 0);
    }

    #[test]
    fn local_price_identity_is_stable_and_schedule_sensitive() {
        let first = ModelCost::new(ModelCostRates {
            input: 1,
            output: 2,
            cache_read: Some(3),
            cache_write_5m: Some(4),
            cache_write_1h: None,
        });
        let changed = ModelCost::new(ModelCostRates {
            input: 2,
            ..first.rates
        });

        let digest = model_cost_sha256(&first);

        assert_eq!(digest, model_cost_sha256(&first));
        assert_eq!(digest.len(), 64);
        assert!(digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
        assert_ne!(digest, model_cost_sha256(&changed));
    }

    fn quote_input(cost: &ModelCost, tokens: u64) -> u64 {
        cost.quote(&priced_usage(tokens, tokens, 0, 0, 0))
            .expect("quote")
            .total_picos
    }

    fn priced_usage(
        input_tokens: u64,
        uncached_input_tokens: u64,
        cached_tokens: u64,
        cache_write_5m_tokens: u64,
        cache_write_1h_tokens: u64,
    ) -> crate::Usage {
        crate::Usage {
            input_tokens,
            output_tokens: 0,
            uncached_input_tokens: Some(uncached_input_tokens),
            cached_tokens: Some(cached_tokens),
            cache_write_5m_tokens: Some(cache_write_5m_tokens),
            cache_write_1h_tokens: Some(cache_write_1h_tokens),
            reasoning_tokens: None,
        }
    }
}
