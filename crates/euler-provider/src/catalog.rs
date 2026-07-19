//! Built-in provider metadata and runtime model-route parsing.

mod official;

use serde_json::Value;
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

/// Millionths of a USD per one million tokens. One rate unit therefore
/// becomes one pico-dollar per token, preserving six-decimal catalog rates
/// while keeping session accumulation deterministic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelCostRates {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelCostTier {
    pub input_tokens_above: u64,
    pub rates: ModelCostRates,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelCost {
    pub rates: ModelCostRates,
    pub tier: Option<ModelCostTier>,
}

impl ModelCost {
    pub const fn new(input: f64, output: f64, cache_read: f64, cache_write: f64) -> Self {
        Self {
            rates: ModelCostRates {
                input: (input * 1_000_000.0 + 0.5) as u64,
                output: (output * 1_000_000.0 + 0.5) as u64,
                cache_read: (cache_read * 1_000_000.0 + 0.5) as u64,
                cache_write: (cache_write * 1_000_000.0 + 0.5) as u64,
            },
            tier: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub const fn with_tier(
        input: f64,
        output: f64,
        cache_read: f64,
        cache_write: f64,
        input_tokens_above: u64,
        tier_input: f64,
        tier_output: f64,
        tier_cache_read: f64,
        tier_cache_write: f64,
    ) -> Self {
        Self {
            rates: Self::new(input, output, cache_read, cache_write).rates,
            tier: Some(ModelCostTier {
                input_tokens_above,
                rates: Self::new(tier_input, tier_output, tier_cache_read, tier_cache_write).rates,
            }),
        }
    }

    pub fn rates_for_input(self, input_tokens: u64) -> ModelCostRates {
        self.tier
            .filter(|tier| input_tokens > tier.input_tokens_above)
            .map_or(self.rates, |tier| tier.rates)
    }
}

pub fn cost_rate_dollars_per_million(rate: u64) -> f64 {
    rate as f64 / 1_000_000.0
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

    pub fn cost(&self) -> Option<ModelCost> {
        self.cost
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

#[allow(clippy::too_many_lines)] // auditable static transcription of pi's catalog
fn pi_catalog_cost(provider: &str, model: &str) -> Option<ModelCost> {
    let simple = |input, output, cache_read, cache_write| {
        ModelCost::new(input, output, cache_read, cache_write)
    };
    let tiered = |input, output, cache_read, tier_input, tier_output, tier_cache_read| {
        ModelCost::with_tier(
            input,
            output,
            cache_read,
            0.0,
            272_000,
            tier_input,
            tier_output,
            tier_cache_read,
            0.0,
        )
    };
    match (provider, model) {
        (ANTHROPIC_PROVIDER_ID, "claude-fable-5") => Some(simple(10.0, 50.0, 1.0, 12.5)),
        (ANTHROPIC_PROVIDER_ID, "claude-haiku-4-5" | "claude-haiku-4-5-20251001") => {
            Some(simple(1.0, 5.0, 0.1, 1.25))
        }
        (ANTHROPIC_PROVIDER_ID, "claude-opus-4-1" | "claude-opus-4-1-20250805") => {
            Some(simple(15.0, 75.0, 1.5, 18.75))
        }
        (
            ANTHROPIC_PROVIDER_ID,
            "claude-opus-4-5"
            | "claude-opus-4-5-20251101"
            | "claude-opus-4-6"
            | "claude-opus-4-7"
            | "claude-opus-4-8",
        ) => Some(simple(5.0, 25.0, 0.5, 6.25)),
        (
            ANTHROPIC_PROVIDER_ID,
            "claude-sonnet-4-5" | "claude-sonnet-4-5-20250929" | "claude-sonnet-4-6",
        ) => Some(simple(3.0, 15.0, 0.3, 3.75)),
        (ANTHROPIC_PROVIDER_ID, "claude-sonnet-5") => Some(simple(2.0, 10.0, 0.2, 2.5)),

        (OPENAI_PROVIDER_ID, "gpt-4") => Some(simple(30.0, 60.0, 0.0, 0.0)),
        (OPENAI_PROVIDER_ID, "gpt-4-turbo") => Some(simple(10.0, 30.0, 0.0, 0.0)),
        (OPENAI_PROVIDER_ID, "gpt-4.1") => Some(simple(2.0, 8.0, 0.5, 0.0)),
        (OPENAI_PROVIDER_ID, "gpt-4.1-mini") => Some(simple(0.4, 1.6, 0.1, 0.0)),
        (OPENAI_PROVIDER_ID, "gpt-4.1-nano") => Some(simple(0.1, 0.4, 0.025, 0.0)),
        (OPENAI_PROVIDER_ID, "gpt-4o") => Some(simple(2.5, 10.0, 1.25, 0.0)),
        (OPENAI_PROVIDER_ID, "gpt-4o-2024-05-13") => Some(simple(5.0, 15.0, 0.0, 0.0)),
        (OPENAI_PROVIDER_ID, "gpt-4o-2024-08-06" | "gpt-4o-2024-11-20") => {
            Some(simple(2.5, 10.0, 1.25, 0.0))
        }
        (OPENAI_PROVIDER_ID, "gpt-4o-mini") => Some(simple(0.15, 0.6, 0.075, 0.0)),
        (
            OPENAI_PROVIDER_ID,
            "gpt-5"
            | "gpt-5-chat-latest"
            | "gpt-5-codex"
            | "gpt-5.1"
            | "gpt-5.1-chat-latest"
            | "gpt-5.1-codex"
            | "gpt-5.1-codex-max",
        ) => Some(simple(1.25, 10.0, 0.125, 0.0)),
        (OPENAI_PROVIDER_ID, "gpt-5-mini" | "gpt-5.1-codex-mini") => {
            Some(simple(0.25, 2.0, 0.025, 0.0))
        }
        (OPENAI_PROVIDER_ID, "gpt-5-nano") => Some(simple(0.05, 0.4, 0.005, 0.0)),
        (OPENAI_PROVIDER_ID, "gpt-5-pro") => Some(simple(15.0, 120.0, 0.0, 0.0)),
        (
            OPENAI_PROVIDER_ID,
            "gpt-5.2"
            | "gpt-5.2-chat-latest"
            | "gpt-5.2-codex"
            | "gpt-5.3-chat-latest"
            | "gpt-5.3-codex"
            | "gpt-5.3-codex-spark",
        ) => Some(simple(1.75, 14.0, 0.175, 0.0)),
        (OPENAI_PROVIDER_ID, "gpt-5.2-pro") => Some(simple(21.0, 168.0, 0.0, 0.0)),
        (OPENAI_PROVIDER_ID, "gpt-5.4") => Some(tiered(2.5, 15.0, 0.25, 5.0, 22.5, 0.5)),
        (OPENAI_PROVIDER_ID, "gpt-5.4-mini") => Some(simple(0.75, 4.5, 0.075, 0.0)),
        (OPENAI_PROVIDER_ID, "gpt-5.4-nano") => Some(simple(0.2, 1.25, 0.02, 0.0)),
        (OPENAI_PROVIDER_ID, "gpt-5.4-pro" | "gpt-5.5-pro") => {
            Some(tiered(30.0, 180.0, 0.0, 60.0, 270.0, 0.0))
        }
        (OPENAI_PROVIDER_ID, "gpt-5.5") => Some(tiered(5.0, 30.0, 0.5, 10.0, 45.0, 1.0)),
        (OPENAI_PROVIDER_ID, "gpt-5.6-luna") => Some(tiered(1.0, 6.0, 0.1, 2.0, 9.0, 0.2)),
        (OPENAI_PROVIDER_ID, "gpt-5.6-sol") => Some(tiered(5.0, 30.0, 0.5, 10.0, 45.0, 1.0)),
        (OPENAI_PROVIDER_ID, "gpt-5.6-terra") => Some(tiered(2.5, 15.0, 0.25, 5.0, 22.5, 0.5)),
        (OPENAI_PROVIDER_ID, "o1") => Some(simple(15.0, 60.0, 7.5, 0.0)),
        (OPENAI_PROVIDER_ID, "o1-pro") => Some(simple(150.0, 600.0, 0.0, 0.0)),
        (OPENAI_PROVIDER_ID, "o3") => Some(simple(2.0, 8.0, 0.5, 0.0)),
        (OPENAI_PROVIDER_ID, "o3-deep-research") => Some(simple(10.0, 40.0, 2.5, 0.0)),
        (OPENAI_PROVIDER_ID, "o3-mini") => Some(simple(1.1, 4.4, 0.55, 0.0)),
        (OPENAI_PROVIDER_ID, "o3-pro") => Some(simple(20.0, 80.0, 0.0, 0.0)),
        (OPENAI_PROVIDER_ID, "o4-mini") => Some(simple(1.1, 4.4, 0.275, 0.0)),
        (OPENAI_PROVIDER_ID, "o4-mini-deep-research") => Some(simple(2.0, 8.0, 0.5, 0.0)),

        (CHATGPT_PROVIDER_ID, "gpt-5.3-codex-spark") => Some(simple(1.75, 14.0, 0.175, 0.0)),
        (CHATGPT_PROVIDER_ID, "gpt-5.4") => Some(tiered(2.5, 15.0, 0.25, 5.0, 22.5, 0.5)),
        (CHATGPT_PROVIDER_ID, "gpt-5.4-mini") => Some(simple(0.75, 4.5, 0.075, 0.0)),
        (CHATGPT_PROVIDER_ID, "gpt-5.5") => Some(tiered(5.0, 30.0, 0.5, 10.0, 45.0, 1.0)),
        (CHATGPT_PROVIDER_ID, "gpt-5.6-luna") => Some(tiered(1.0, 6.0, 0.1, 2.0, 9.0, 0.2)),
        (CHATGPT_PROVIDER_ID, "gpt-5.6-sol") => Some(tiered(5.0, 30.0, 0.5, 10.0, 45.0, 1.0)),
        (CHATGPT_PROVIDER_ID, "gpt-5.6-terra") => Some(tiered(2.5, 15.0, 0.25, 5.0, 22.5, 0.5)),

        (OPENROUTER_PROVIDER_ID, "openai/gpt-4.1-mini") => Some(simple(0.4, 1.6, 0.1, 0.0)),
        (OPENROUTER_PROVIDER_ID, "openai/gpt-5.5") => Some(simple(5.0, 30.0, 0.5, 0.0)),
        (OPENROUTER_PROVIDER_ID, "anthropic/claude-sonnet-5") => Some(simple(2.0, 10.0, 0.2, 0.0)),
        (OPENROUTER_PROVIDER_ID, "z-ai/glm-5.2") => Some(simple(0.4144, 1.3024, 0.07696, 0.0)),

        (XAI_PROVIDER_ID, "grok-3") => Some(simple(3.0, 15.0, 0.75, 0.0)),
        (XAI_PROVIDER_ID, "grok-3-fast") => Some(simple(5.0, 25.0, 1.25, 0.0)),
        (
            XAI_PROVIDER_ID,
            "grok-4.20-0309-non-reasoning" | "grok-4.20-0309-reasoning" | "grok-4.3",
        ) => Some(simple(1.25, 2.5, 0.2, 0.0)),
        (XAI_PROVIDER_ID, "grok-4.5") => Some(simple(2.0, 6.0, 0.5, 0.0)),
        (XAI_PROVIDER_ID, "grok-build-0.1") => Some(simple(1.0, 2.0, 0.2, 0.0)),
        (XAI_PROVIDER_ID, "grok-code-fast-1") => Some(simple(0.2, 1.5, 0.02, 0.0)),
        _ => None,
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
                let cost = pi_catalog_cost(&provider_id, &model.id);
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
        Ok(Self { providers })
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

#[allow(clippy::too_many_lines)] // pricing adds one validated metadata branch
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
                "cost",
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
        let display_name = local_display_name(object, &id, &scope, warnings);
        let context_window_tokens =
            optional_positive_u64(object, "context_window_tokens", &scope, warnings);
        let max_output_tokens =
            optional_positive_u64(object, "max_output_tokens", &scope, warnings);
        let supports_tools = optional_bool(object, "supports_tools", &scope, warnings);
        let supports_reasoning = optional_bool(object, "supports_reasoning", &scope, warnings);
        let cost = parse_model_cost(object, &scope, warnings);
        let (effective_context_window_percent, auto_compact_token_limit) =
            if provider_key == CHATGPT_PROVIDER_ID {
                chatgpt_context_policy(context_window_tokens)
            } else {
                (None, None)
            };
        let (reasoning_efforts, official_route) = provider
            .models
            .get(&id)
            .map(|model| (model.reasoning_efforts.clone(), model.official_route))
            .unwrap_or_default();
        if provider
            .models
            .get(&id)
            .is_some_and(|model| model.source == ModelDescriptorSource::Local)
        {
            warnings.push(format!(
                "provider `{provider_key}` model `{id}` appeared more than once; last valid descriptor wins"
            ));
        }
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
                reasoning_efforts,
                official_route,
                effective_context_window_percent,
                auto_compact_token_limit,
                cost,
            },
        );
    }
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
) -> Option<ModelCost> {
    let value = object.get("cost")?;
    let Some(cost) = value.as_object() else {
        warnings.push(format!("ignored {scope} cost because it must be an object"));
        return None;
    };
    let input = cost_rate(cost, "input", scope, warnings)?;
    let output = cost_rate(cost, "output", scope, warnings)?;
    let cache_read = optional_cost_rate(cost, "cache_read", scope, warnings)?;
    let cache_write = optional_cost_rate(cost, "cache_write", scope, warnings)?;
    let base = ModelCost::new(input, output, cache_read, cache_write);
    let Some(tier) = cost
        .get("tiers")
        .and_then(Value::as_array)
        .and_then(|tiers| tiers.first())
        .and_then(Value::as_object)
    else {
        return Some(base);
    };
    let threshold = tier.get("input_tokens_above").and_then(Value::as_u64)?;
    let tier_input = cost_rate(tier, "input", scope, warnings)?;
    let tier_output = cost_rate(tier, "output", scope, warnings)?;
    let tier_cache_read = optional_cost_rate(tier, "cache_read", scope, warnings)?;
    let tier_cache_write = optional_cost_rate(tier, "cache_write", scope, warnings)?;
    Some(ModelCost::with_tier(
        input,
        output,
        cache_read,
        cache_write,
        threshold,
        tier_input,
        tier_output,
        tier_cache_read,
        tier_cache_write,
    ))
}

fn cost_rate(
    object: &serde_json::Map<String, Value>,
    field: &str,
    scope: &str,
    warnings: &mut Vec<String>,
) -> Option<f64> {
    let rate = object
        .get(field)
        .and_then(Value::as_f64)
        .filter(|rate| rate.is_finite() && *rate >= 0.0);
    if rate.is_none() {
        warnings.push(format!(
            "ignored {scope} cost because {field} must be a non-negative number"
        ));
    }
    rate
}

fn optional_cost_rate(
    object: &serde_json::Map<String, Value>,
    field: &str,
    scope: &str,
    warnings: &mut Vec<String>,
) -> Option<f64> {
    if object.contains_key(field) {
        cost_rate(object, field, scope, warnings)
    } else {
        Some(0.0)
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
            &[ReasoningEffort::Max]
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
    fn xai_built_in_models_pin_pi_reference_and_routable_default() {
        let catalog = MergedModelCatalog::built_in();
        let xai = catalog.provider(XAI_PROVIDER_ID).expect("xai provider");
        assert_eq!(xai.models().count(), 8);
        let default = xai
            .models()
            .find(|model| model.id() == DEFAULT_XAI_MODEL)
            .expect("default xai model listed");
        assert_eq!(default.display_name(), "Grok 4.3");
        assert_eq!(default.supports_reasoning(), Some(true));
        assert!(built_in_model_supports_reasoning("xai", DEFAULT_XAI_MODEL));

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
}
