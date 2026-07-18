//! Built-in provider metadata and runtime model-route parsing.

use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

use crate::ReasoningEffort;

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
const EULER_MODELS_REFRESH_GENERATOR: &str = "euler models refresh";

const STANDARD_REASONING_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::XSmall,
    ReasoningEffort::Small,
    ReasoningEffort::Medium,
    ReasoningEffort::Large,
    ReasoningEffort::XLarge,
];
const MAX_REASONING_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::XSmall,
    ReasoningEffort::Small,
    ReasoningEffort::Medium,
    ReasoningEffort::Large,
    ReasoningEffort::XLarge,
    ReasoningEffort::Max,
];
const MAX_ONLY_REASONING_EFFORTS: &[ReasoningEffort] = &[ReasoningEffort::Max];
const INKLING_REASONING_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::XSmall,
    ReasoningEffort::Small,
    ReasoningEffort::Medium,
    ReasoningEffort::Large,
    ReasoningEffort::Max,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltInModelDescriptor {
    pub id: &'static str,
    pub display_name: &'static str,
    pub context_window_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub supports_tools: Option<bool>,
    pub supports_reasoning: Option<bool>,
    pub effective_context_window_percent: Option<u8>,
    pub auto_compact_token_limit: Option<u64>,
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
    effective_context_window_percent: Option<u8>,
    auto_compact_token_limit: Option<u64>,
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
            effective_context_window_percent: None,
            auto_compact_token_limit: None,
        }],
    },
    ProviderDescriptor {
        id: CHATGPT_PROVIDER_ID,
        display_name: "ChatGPT",
        default_model: DEFAULT_CHATGPT_MODEL,
        aliases: &[],
        auth_file_supported: true,
        models: CHATGPT_MODELS,
    },
    ProviderDescriptor {
        id: OPENAI_PROVIDER_ID,
        display_name: "OpenAI",
        default_model: DEFAULT_OPENAI_MODEL,
        aliases: &[],
        auth_file_supported: true,
        models: OPENAI_MODELS,
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
    ProviderDescriptor {
        id: XAI_PROVIDER_ID,
        display_name: "xAI",
        default_model: DEFAULT_XAI_MODEL,
        aliases: &[],
        auth_file_supported: true,
        models: XAI_MODELS,
    },
];

pub(crate) const ANTHROPIC_MODELS: &[BuiltInModelDescriptor] = &[
    built_in_model("claude-fable-5", "Claude Fable 5", 1_000_000, 128_000, true),
    built_in_model(
        "claude-haiku-4-5",
        "Claude Haiku 4.5 (latest)",
        200_000,
        64_000,
        true,
    ),
    built_in_model(
        "claude-haiku-4-5-20251001",
        "Claude Haiku 4.5",
        200_000,
        64_000,
        true,
    ),
    built_in_model(
        "claude-opus-4-1",
        "Claude Opus 4.1 (latest)",
        200_000,
        32_000,
        true,
    ),
    built_in_model(
        "claude-opus-4-1-20250805",
        "Claude Opus 4.1",
        200_000,
        32_000,
        true,
    ),
    built_in_model(
        "claude-opus-4-5",
        "Claude Opus 4.5 (latest)",
        200_000,
        64_000,
        true,
    ),
    built_in_model(
        "claude-opus-4-5-20251101",
        "Claude Opus 4.5",
        200_000,
        64_000,
        true,
    ),
    built_in_model(
        "claude-opus-4-6",
        "Claude Opus 4.6",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "claude-opus-4-7",
        "Claude Opus 4.7",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "claude-opus-4-8",
        "Claude Opus 4.8",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "claude-sonnet-4-5",
        "Claude Sonnet 4.5 (latest)",
        1_000_000,
        64_000,
        true,
    ),
    built_in_model(
        "claude-sonnet-4-5-20250929",
        "Claude Sonnet 4.5",
        1_000_000,
        64_000,
        true,
    ),
    built_in_model(
        "claude-sonnet-4-6",
        "Claude Sonnet 4.6",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "claude-sonnet-5",
        "Claude Sonnet 5",
        1_000_000,
        128_000,
        true,
    ),
];

/// OpenAI platform API models (pi reference: openai.models.ts).
const OPENAI_MODELS: &[BuiltInModelDescriptor] = &[
    built_in_model("gpt-4", "GPT-4", 8_192, 8_192, false),
    built_in_model("gpt-4-turbo", "GPT-4 Turbo", 128_000, 4_096, false),
    built_in_model("gpt-4.1", "GPT-4.1", 1_047_576, 32_768, false),
    built_in_model("gpt-4.1-mini", "GPT-4.1 mini", 1_047_576, 32_768, false),
    built_in_model("gpt-4.1-nano", "GPT-4.1 nano", 1_047_576, 32_768, false),
    built_in_model("gpt-4o", "GPT-4o", 128_000, 16_384, false),
    built_in_model(
        "gpt-4o-2024-05-13",
        "GPT-4o (2024-05-13)",
        128_000,
        4_096,
        false,
    ),
    built_in_model(
        "gpt-4o-2024-08-06",
        "GPT-4o (2024-08-06)",
        128_000,
        16_384,
        false,
    ),
    built_in_model(
        "gpt-4o-2024-11-20",
        "GPT-4o (2024-11-20)",
        128_000,
        16_384,
        false,
    ),
    built_in_model("gpt-4o-mini", "GPT-4o mini", 128_000, 16_384, false),
    built_in_model("gpt-5", "GPT-5", 400_000, 128_000, true),
    built_in_model(
        "gpt-5-chat-latest",
        "GPT-5 Chat Latest",
        128_000,
        16_384,
        false,
    ),
    built_in_model("gpt-5-codex", "GPT-5-Codex", 400_000, 128_000, true),
    built_in_model("gpt-5-mini", "GPT-5 Mini", 400_000, 128_000, true),
    built_in_model("gpt-5-nano", "GPT-5 Nano", 400_000, 128_000, true),
    built_in_model("gpt-5-pro", "GPT-5 Pro", 400_000, 128_000, true),
    built_in_model("gpt-5.1", "GPT-5.1", 400_000, 128_000, true),
    built_in_model("gpt-5.1-chat-latest", "GPT-5.1 Chat", 128_000, 16_384, true),
    built_in_model("gpt-5.1-codex", "GPT-5.1 Codex", 400_000, 128_000, true),
    built_in_model(
        "gpt-5.1-codex-max",
        "GPT-5.1 Codex Max",
        400_000,
        128_000,
        true,
    ),
    built_in_model(
        "gpt-5.1-codex-mini",
        "GPT-5.1 Codex mini",
        400_000,
        128_000,
        true,
    ),
    built_in_model("gpt-5.2", "GPT-5.2", 400_000, 128_000, true),
    built_in_model("gpt-5.2-chat-latest", "GPT-5.2 Chat", 128_000, 16_384, true),
    built_in_model("gpt-5.2-codex", "GPT-5.2 Codex", 400_000, 128_000, true),
    built_in_model("gpt-5.2-pro", "GPT-5.2 Pro", 400_000, 128_000, true),
    built_in_model(
        "gpt-5.3-chat-latest",
        "GPT-5.3 Chat (latest)",
        128_000,
        16_384,
        false,
    ),
    built_in_model("gpt-5.3-codex", "GPT-5.3 Codex", 400_000, 128_000, true),
    built_in_model(
        "gpt-5.3-codex-spark",
        "GPT-5.3 Codex Spark",
        128_000,
        32_000,
        true,
    ),
    built_in_model("gpt-5.4", "GPT-5.4", 272_000, 128_000, true),
    built_in_model("gpt-5.4-mini", "GPT-5.4 mini", 400_000, 128_000, true),
    built_in_model("gpt-5.4-nano", "GPT-5.4 nano", 400_000, 128_000, true),
    built_in_model("gpt-5.4-pro", "GPT-5.4 Pro", 1_050_000, 128_000, true),
    built_in_model("gpt-5.5", "GPT-5.5", 272_000, 128_000, true),
    built_in_model("gpt-5.5-pro", "GPT-5.5 Pro", 1_050_000, 128_000, true),
    built_in_model("gpt-5.6-luna", "GPT-5.6 Luna", 272_000, 128_000, true),
    built_in_model("gpt-5.6-sol", "GPT-5.6 Sol", 272_000, 128_000, true),
    built_in_model("gpt-5.6-terra", "GPT-5.6 Terra", 272_000, 128_000, true),
    built_in_model("o1", "o1", 200_000, 100_000, true),
    built_in_model("o1-pro", "o1-pro", 200_000, 100_000, true),
    built_in_model("o3", "o3", 200_000, 100_000, true),
    built_in_model(
        "o3-deep-research",
        "o3-deep-research",
        200_000,
        100_000,
        true,
    ),
    built_in_model("o3-mini", "o3-mini", 200_000, 100_000, true),
    built_in_model("o3-pro", "o3-pro", 200_000, 100_000, true),
    built_in_model("o4-mini", "o4-mini", 200_000, 100_000, true),
    built_in_model(
        "o4-mini-deep-research",
        "o4-mini-deep-research",
        200_000,
        100_000,
        true,
    ),
];

/// ChatGPT-subscription backend models (pi reference: openai-codex.models.ts —
/// the codex backend exposes a different, smaller set than the platform API).
/// Route-specific operational limits stay here rather than being copied from
/// the OpenAI API catalog. Codex reserves 5% of each raw window for runtime
/// headroom and compacts automatically at 90% of the raw window.
const CHATGPT_STANDARD_CONTEXT_WINDOW_TOKENS: u64 = 272_000;
const CHATGPT_GPT56_CONTEXT_WINDOW_TOKENS: u64 = 372_000;
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

const CHATGPT_MODELS: &[BuiltInModelDescriptor] = &[
    chatgpt_model("gpt-5.3-codex-spark", "GPT-5.3 Codex Spark", 128_000),
    chatgpt_model("gpt-5.4", "GPT-5.4", CHATGPT_STANDARD_CONTEXT_WINDOW_TOKENS),
    chatgpt_model(
        "gpt-5.4-mini",
        "GPT-5.4 mini",
        CHATGPT_STANDARD_CONTEXT_WINDOW_TOKENS,
    ),
    chatgpt_model("gpt-5.5", "GPT-5.5", CHATGPT_STANDARD_CONTEXT_WINDOW_TOKENS),
    chatgpt_model(
        "gpt-5.6-luna",
        "GPT-5.6 Luna",
        CHATGPT_GPT56_CONTEXT_WINDOW_TOKENS,
    ),
    chatgpt_model(
        "gpt-5.6-sol",
        "GPT-5.6 Sol",
        CHATGPT_GPT56_CONTEXT_WINDOW_TOKENS,
    ),
    chatgpt_model(
        "gpt-5.6-terra",
        "GPT-5.6 Terra",
        CHATGPT_GPT56_CONTEXT_WINDOW_TOKENS,
    ),
];

const OPENROUTER_MODELS: &[BuiltInModelDescriptor] = &[
    built_in_model(
        "ai21/jamba-large-1.7",
        "AI21: Jamba Large 1.7",
        256_000,
        4_096,
        false,
    ),
    built_in_model(
        "aion-labs/aion-2.0",
        "AionLabs: Aion-2.0",
        131_072,
        32_768,
        true,
    ),
    built_in_model(
        "aion-labs/aion-3.0",
        "AionLabs: Aion-3.0",
        131_072,
        32_768,
        true,
    ),
    built_in_model(
        "aion-labs/aion-3.0-mini",
        "AionLabs: Aion-3.0-Mini",
        131_072,
        32_768,
        true,
    ),
    built_in_model(
        "amazon/nova-2-lite-v1",
        "Amazon: Nova 2 Lite",
        1_000_000,
        65_535,
        true,
    ),
    built_in_model(
        "amazon/nova-lite-v1",
        "Amazon: Nova Lite 1.0",
        300_000,
        5_120,
        false,
    ),
    built_in_model(
        "amazon/nova-micro-v1",
        "Amazon: Nova Micro 1.0",
        128_000,
        5_120,
        false,
    ),
    built_in_model(
        "amazon/nova-premier-v1",
        "Amazon: Nova Premier 1.0",
        1_000_000,
        32_000,
        false,
    ),
    built_in_model(
        "amazon/nova-pro-v1",
        "Amazon: Nova Pro 1.0",
        300_000,
        5_120,
        false,
    ),
    built_in_model(
        "anthropic/claude-3-haiku",
        "Anthropic: Claude 3 Haiku",
        200_000,
        4_096,
        false,
    ),
    built_in_model(
        "anthropic/claude-fable-5",
        "Anthropic: Claude Fable 5",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "anthropic/claude-haiku-4.5",
        "Anthropic: Claude Haiku 4.5",
        200_000,
        64_000,
        true,
    ),
    built_in_model(
        "anthropic/claude-opus-4",
        "Anthropic: Claude Opus 4",
        200_000,
        32_000,
        true,
    ),
    built_in_model(
        "anthropic/claude-opus-4.1",
        "Anthropic: Claude Opus 4.1",
        200_000,
        32_000,
        true,
    ),
    built_in_model(
        "anthropic/claude-opus-4.5",
        "Anthropic: Claude Opus 4.5",
        200_000,
        64_000,
        true,
    ),
    built_in_model(
        "anthropic/claude-opus-4.6",
        "Anthropic: Claude Opus 4.6",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "anthropic/claude-opus-4.7",
        "Anthropic: Claude Opus 4.7",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "anthropic/claude-opus-4.7-fast",
        "Anthropic: Claude Opus 4.7 (Fast)",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "anthropic/claude-opus-4.8",
        "Anthropic: Claude Opus 4.8",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "anthropic/claude-opus-4.8-fast",
        "Anthropic: Claude Opus 4.8 (Fast)",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "anthropic/claude-sonnet-4",
        "Anthropic: Claude Sonnet 4",
        1_000_000,
        64_000,
        true,
    ),
    built_in_model(
        "anthropic/claude-sonnet-4.5",
        "Anthropic: Claude Sonnet 4.5",
        1_000_000,
        64_000,
        true,
    ),
    built_in_model(
        "anthropic/claude-sonnet-4.6",
        "Anthropic: Claude Sonnet 4.6",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "anthropic/claude-sonnet-5",
        "Anthropic: Claude Sonnet 5",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "arcee-ai/trinity-large-thinking",
        "Arcee AI: Trinity Large Thinking",
        262_144,
        80_000,
        true,
    ),
    built_in_model(
        "arcee-ai/virtuoso-large",
        "Arcee AI: Virtuoso Large",
        131_072,
        64_000,
        false,
    ),
    built_in_model(
        "bytedance-seed/seed-1.6",
        "ByteDance Seed: Seed 1.6",
        262_144,
        32_768,
        true,
    ),
    built_in_model(
        "bytedance-seed/seed-1.6-flash",
        "ByteDance Seed: Seed 1.6 Flash",
        262_144,
        32_768,
        true,
    ),
    built_in_model(
        "bytedance-seed/seed-2.0-lite",
        "ByteDance Seed: Seed-2.0-Lite",
        262_144,
        131_072,
        true,
    ),
    built_in_model(
        "bytedance-seed/seed-2.0-mini",
        "ByteDance Seed: Seed-2.0-Mini",
        262_144,
        131_072,
        true,
    ),
    built_in_model(
        "cohere/command-r-08-2024",
        "Cohere: Command R (08-2024)",
        128_000,
        4_000,
        false,
    ),
    built_in_model(
        "cohere/command-r-plus-08-2024",
        "Cohere: Command R+ (08-2024)",
        128_000,
        4_000,
        false,
    ),
    built_in_model(
        "cohere/north-mini-code:free",
        "Cohere: North Mini Code (free)",
        256_000,
        64_000,
        true,
    ),
    built_in_model(
        "deepseek/deepseek-chat",
        "DeepSeek: DeepSeek V3",
        128_000,
        16_000,
        false,
    ),
    built_in_model(
        "deepseek/deepseek-chat-v3-0324",
        "DeepSeek: DeepSeek V3 0324",
        163_840,
        16_384,
        false,
    ),
    built_in_model(
        "deepseek/deepseek-chat-v3.1",
        "DeepSeek: DeepSeek V3.1",
        163_840,
        32_768,
        true,
    ),
    built_in_model("deepseek/deepseek-r1", "DeepSeek: R1", 64_000, 16_000, true),
    built_in_model(
        "deepseek/deepseek-r1-0528",
        "DeepSeek: R1 0528",
        163_840,
        32_768,
        true,
    ),
    built_in_model(
        "deepseek/deepseek-v3.1-terminus",
        "DeepSeek: DeepSeek V3.1 Terminus",
        163_840,
        32_768,
        true,
    ),
    built_in_model(
        "deepseek/deepseek-v3.2",
        "DeepSeek: DeepSeek V3.2",
        128_000,
        64_000,
        true,
    ),
    built_in_model(
        "deepseek/deepseek-v3.2-exp",
        "DeepSeek: DeepSeek V3.2 Exp",
        163_840,
        65_536,
        true,
    ),
    built_in_model(
        "deepseek/deepseek-v4-flash",
        "DeepSeek: DeepSeek V4 Flash",
        1_048_576,
        65_536,
        true,
    ),
    built_in_model(
        "deepseek/deepseek-v4-pro",
        "DeepSeek: DeepSeek V4 Pro",
        1_048_576,
        384_000,
        true,
    ),
    built_in_model(
        "google/gemini-2.5-flash",
        "Google: Gemini 2.5 Flash",
        1_048_576,
        65_535,
        true,
    ),
    built_in_model(
        "google/gemini-2.5-flash-lite",
        "Google: Gemini 2.5 Flash Lite",
        1_048_576,
        65_535,
        true,
    ),
    built_in_model(
        "google/gemini-2.5-pro",
        "Google: Gemini 2.5 Pro",
        1_048_576,
        65_536,
        true,
    ),
    built_in_model(
        "google/gemini-2.5-pro-preview",
        "Google: Gemini 2.5 Pro Preview 06-05",
        1_048_576,
        65_536,
        true,
    ),
    built_in_model(
        "google/gemini-2.5-pro-preview-05-06",
        "Google: Gemini 2.5 Pro Preview 05-06",
        1_048_576,
        65_535,
        true,
    ),
    built_in_model(
        "google/gemini-3-flash-preview",
        "Google: Gemini 3 Flash Preview",
        1_048_576,
        65_535,
        true,
    ),
    built_in_model(
        "google/gemini-3-pro-image",
        "Google: Nano Banana Pro (Gemini 3 Pro Image)",
        65_536,
        32_768,
        true,
    ),
    built_in_model(
        "google/gemini-3.1-flash-lite",
        "Google: Gemini 3.1 Flash Lite",
        1_048_576,
        65_536,
        true,
    ),
    built_in_model(
        "google/gemini-3.1-flash-lite-preview",
        "Google: Gemini 3.1 Flash Lite Preview",
        1_048_576,
        65_536,
        true,
    ),
    built_in_model(
        "google/gemini-3.1-pro-preview",
        "Google: Gemini 3.1 Pro Preview",
        1_048_576,
        65_536,
        true,
    ),
    built_in_model(
        "google/gemini-3.1-pro-preview-customtools",
        "Google: Gemini 3.1 Pro Preview Custom Tools",
        1_048_576,
        65_536,
        true,
    ),
    built_in_model(
        "google/gemini-3.5-flash",
        "Google: Gemini 3.5 Flash",
        1_048_576,
        65_536,
        true,
    ),
    built_in_model(
        "google/gemma-3-12b-it",
        "Google: Gemma 3 12B",
        131_072,
        16_384,
        false,
    ),
    built_in_model(
        "google/gemma-3-27b-it",
        "Google: Gemma 3 27B",
        131_072,
        16_384,
        false,
    ),
    built_in_model(
        "google/gemma-4-26b-a4b-it",
        "Google: Gemma 4 26B A4B ",
        262_144,
        4_096,
        true,
    ),
    built_in_model(
        "google/gemma-4-26b-a4b-it:free",
        "Google: Gemma 4 26B A4B  (free)",
        131_072,
        32_768,
        true,
    ),
    built_in_model(
        "google/gemma-4-31b-it",
        "Google: Gemma 4 31B",
        262_144,
        262_144,
        true,
    ),
    built_in_model(
        "google/gemma-4-31b-it:free",
        "Google: Gemma 4 31B (free)",
        262_144,
        8_192,
        true,
    ),
    built_in_model(
        "ibm-granite/granite-4.1-8b",
        "IBM: Granite 4.1 8B",
        131_072,
        131_072,
        false,
    ),
    built_in_model(
        "inception/mercury-2",
        "Inception: Mercury 2",
        128_000,
        50_000,
        true,
    ),
    built_in_model(
        "inclusionai/ling-2.6-1t",
        "inclusionAI: Ling-2.6-1T",
        262_144,
        32_768,
        false,
    ),
    built_in_model(
        "inclusionai/ling-2.6-flash",
        "inclusionAI: Ling-2.6-flash",
        262_144,
        32_768,
        false,
    ),
    built_in_model(
        "inclusionai/ring-2.6-1t",
        "inclusionAI: Ring-2.6-1T",
        262_144,
        65_536,
        true,
    ),
    built_in_model(
        "kwaipilot/kat-coder-air-v2.5",
        "Kwaipilot: KAT-Coder-Air V2.5",
        256_000,
        80_000,
        false,
    ),
    built_in_model(
        "kwaipilot/kat-coder-pro-v2",
        "Kwaipilot: KAT-Coder-Pro V2",
        256_000,
        80_000,
        false,
    ),
    built_in_model(
        "kwaipilot/kat-coder-pro-v2.5",
        "Kwaipilot: KAT-Coder-Pro V2.5",
        256_000,
        80_000,
        false,
    ),
    built_in_model(
        "meta-llama/llama-3.1-70b-instruct",
        "Meta: Llama 3.1 70B Instruct",
        131_072,
        16_384,
        false,
    ),
    built_in_model(
        "meta-llama/llama-3.1-8b-instruct",
        "Meta: Llama 3.1 8B Instruct",
        131_072,
        16_384,
        false,
    ),
    built_in_model(
        "meta-llama/llama-3.3-70b-instruct",
        "Meta: Llama 3.3 70B Instruct",
        131_072,
        16_384,
        false,
    ),
    built_in_model(
        "meta-llama/llama-3.3-70b-instruct:free",
        "Meta: Llama 3.3 70B Instruct (free)",
        65_536,
        4_096,
        false,
    ),
    built_in_model(
        "meta-llama/llama-4-maverick",
        "Meta: Llama 4 Maverick",
        1_048_576,
        16_384,
        false,
    ),
    built_in_model(
        "meta-llama/llama-4-scout",
        "Meta: Llama 4 Scout",
        327_680,
        16_384,
        false,
    ),
    built_in_model(
        "meta/muse-spark-1.1",
        "Meta: Muse Spark 1.1",
        1_048_576,
        1_048_576,
        true,
    ),
    built_in_model(
        "minimax/minimax-m1",
        "MiniMax: MiniMax M1",
        1_000_000,
        40_000,
        true,
    ),
    built_in_model(
        "minimax/minimax-m2",
        "MiniMax: MiniMax M2",
        204_800,
        131_072,
        true,
    ),
    built_in_model(
        "minimax/minimax-m2.1",
        "MiniMax: MiniMax M2.1",
        204_800,
        131_072,
        true,
    ),
    built_in_model(
        "minimax/minimax-m2.5",
        "MiniMax: MiniMax M2.5",
        196_608,
        196_608,
        true,
    ),
    built_in_model(
        "minimax/minimax-m2.7",
        "MiniMax: MiniMax M2.7",
        196_608,
        196_608,
        true,
    ),
    built_in_model(
        "minimax/minimax-m3",
        "MiniMax: MiniMax M3",
        1_000_000,
        131_072,
        true,
    ),
    built_in_model(
        "mistralai/codestral-2508",
        "Mistral: Codestral 2508",
        256_000,
        4_096,
        false,
    ),
    built_in_model(
        "mistralai/devstral-2512",
        "Mistral: Devstral 2 2512",
        262_144,
        4_096,
        false,
    ),
    built_in_model(
        "mistralai/ministral-14b-2512",
        "Mistral: Ministral 3 14B 2512",
        262_144,
        4_096,
        false,
    ),
    built_in_model(
        "mistralai/ministral-3b-2512",
        "Mistral: Ministral 3 3B 2512",
        131_072,
        4_096,
        false,
    ),
    built_in_model(
        "mistralai/ministral-8b-2512",
        "Mistral: Ministral 3 8B 2512",
        262_144,
        4_096,
        false,
    ),
    built_in_model(
        "mistralai/mistral-large",
        "Mistral Large",
        128_000,
        4_096,
        false,
    ),
    built_in_model(
        "mistralai/mistral-large-2407",
        "Mistral Large 2407",
        131_072,
        4_096,
        false,
    ),
    built_in_model(
        "mistralai/mistral-large-2512",
        "Mistral: Mistral Large 3 2512",
        262_144,
        4_096,
        false,
    ),
    built_in_model(
        "mistralai/mistral-medium-3",
        "Mistral: Mistral Medium 3",
        131_072,
        4_096,
        false,
    ),
    built_in_model(
        "mistralai/mistral-medium-3-5",
        "Mistral: Mistral Medium 3.5",
        262_144,
        4_096,
        true,
    ),
    built_in_model(
        "mistralai/mistral-medium-3.1",
        "Mistral: Mistral Medium 3.1",
        131_072,
        4_096,
        false,
    ),
    built_in_model(
        "mistralai/mistral-nemo",
        "Mistral: Mistral Nemo",
        131_072,
        4_096,
        false,
    ),
    built_in_model(
        "mistralai/mistral-saba",
        "Mistral: Saba",
        32_768,
        4_096,
        false,
    ),
    built_in_model(
        "mistralai/mistral-small-2603",
        "Mistral: Mistral Small 4",
        262_144,
        4_096,
        true,
    ),
    built_in_model(
        "mistralai/mistral-small-3.2-24b-instruct",
        "Mistral: Mistral Small 3.2 24B",
        128_000,
        16_384,
        false,
    ),
    built_in_model(
        "mistralai/mixtral-8x22b-instruct",
        "Mistral: Mixtral 8x22B Instruct",
        65_536,
        4_096,
        false,
    ),
    built_in_model(
        "mistralai/voxtral-small-24b-2507",
        "Mistral: Voxtral Small 24B 2507",
        32_000,
        4_096,
        false,
    ),
    built_in_model(
        "moonshotai/kimi-k2",
        "MoonshotAI: Kimi K2 0711",
        131_072,
        100_352,
        false,
    ),
    built_in_model(
        "moonshotai/kimi-k2-0905",
        "MoonshotAI: Kimi K2 0905",
        262_144,
        100_352,
        false,
    ),
    built_in_model(
        "moonshotai/kimi-k2-thinking",
        "MoonshotAI: Kimi K2 Thinking",
        262_144,
        100_352,
        true,
    ),
    built_in_model(
        "moonshotai/kimi-k2.5",
        "MoonshotAI: Kimi K2.5",
        256_000,
        4_096,
        true,
    ),
    built_in_model(
        "moonshotai/kimi-k2.6",
        "MoonshotAI: Kimi K2.6",
        262_144,
        262_144,
        true,
    ),
    built_in_model(
        "moonshotai/kimi-k2.7-code",
        "MoonshotAI: Kimi K2.7 Code",
        262_144,
        262_144,
        true,
    ),
    built_in_model(
        "moonshotai/kimi-k3",
        "MoonshotAI: Kimi K3",
        1_048_576,
        1_048_576,
        true,
    ),
    built_in_model(
        "nex-agi/nex-n2-mini",
        "Nex AGI: Nex-N2-Mini",
        262_144,
        262_144,
        true,
    ),
    built_in_model(
        "nex-agi/nex-n2-pro",
        "Nex AGI: Nex-N2-Pro",
        262_144,
        262_144,
        true,
    ),
    built_in_model(
        "nvidia/nemotron-3-nano-30b-a3b",
        "NVIDIA: Nemotron 3 Nano 30B A3B",
        262_144,
        228_000,
        true,
    ),
    built_in_model(
        "nvidia/nemotron-3-nano-30b-a3b:free",
        "NVIDIA: Nemotron 3 Nano 30B A3B (free)",
        256_000,
        4_096,
        true,
    ),
    built_in_model(
        "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free",
        "NVIDIA: Nemotron 3 Nano Omni (free)",
        256_000,
        65_536,
        false,
    ),
    built_in_model(
        "nvidia/nemotron-3-super-120b-a12b",
        "NVIDIA: Nemotron 3 Super",
        262_144,
        4_096,
        true,
    ),
    built_in_model(
        "nvidia/nemotron-3-super-120b-a12b:free",
        "NVIDIA: Nemotron 3 Super (free)",
        262_144,
        262_144,
        true,
    ),
    built_in_model(
        "nvidia/nemotron-3-ultra-550b-a55b",
        "NVIDIA: Nemotron 3 Ultra",
        262_144,
        16_384,
        true,
    ),
    built_in_model(
        "nvidia/nemotron-3-ultra-550b-a55b:free",
        "NVIDIA: Nemotron 3 Ultra (free)",
        1_000_000,
        65_536,
        true,
    ),
    built_in_model(
        "nvidia/nemotron-nano-12b-v2-vl:free",
        "NVIDIA: Nemotron Nano 12B 2 VL (free)",
        128_000,
        128_000,
        true,
    ),
    built_in_model(
        "nvidia/nemotron-nano-9b-v2:free",
        "NVIDIA: Nemotron Nano 9B V2 (free)",
        128_000,
        4_096,
        true,
    ),
    built_in_model(
        "openai/gpt-3.5-turbo",
        "OpenAI: GPT-3.5 Turbo",
        16_385,
        4_096,
        false,
    ),
    built_in_model(
        "openai/gpt-3.5-turbo-0613",
        "OpenAI: GPT-3.5 Turbo (older v0613)",
        4_095,
        4_096,
        false,
    ),
    built_in_model(
        "openai/gpt-3.5-turbo-16k",
        "OpenAI: GPT-3.5 Turbo 16k",
        16_385,
        4_096,
        false,
    ),
    built_in_model("openai/gpt-4", "OpenAI: GPT-4", 8_191, 4_096, false),
    built_in_model(
        "openai/gpt-4-turbo",
        "OpenAI: GPT-4 Turbo",
        128_000,
        4_096,
        false,
    ),
    built_in_model(
        "openai/gpt-4-turbo-preview",
        "OpenAI: GPT-4 Turbo Preview",
        128_000,
        4_096,
        false,
    ),
    built_in_model("openai/gpt-4.1", "OpenAI: GPT-4.1", 1_047_576, 4_096, false),
    built_in_model(
        "openai/gpt-4.1-mini",
        "OpenAI: GPT-4.1 Mini",
        1_047_576,
        32_768,
        false,
    ),
    built_in_model(
        "openai/gpt-4.1-nano",
        "OpenAI: GPT-4.1 Nano",
        1_047_576,
        32_768,
        false,
    ),
    built_in_model("openai/gpt-4o", "OpenAI: GPT-4o", 128_000, 16_384, false),
    built_in_model(
        "openai/gpt-4o-2024-05-13",
        "OpenAI: GPT-4o (2024-05-13)",
        128_000,
        4_096,
        false,
    ),
    built_in_model(
        "openai/gpt-4o-2024-08-06",
        "OpenAI: GPT-4o (2024-08-06)",
        128_000,
        16_384,
        false,
    ),
    built_in_model(
        "openai/gpt-4o-2024-11-20",
        "OpenAI: GPT-4o (2024-11-20)",
        128_000,
        16_384,
        false,
    ),
    built_in_model(
        "openai/gpt-4o-mini",
        "OpenAI: GPT-4o-mini",
        128_000,
        16_384,
        false,
    ),
    built_in_model(
        "openai/gpt-4o-mini-2024-07-18",
        "OpenAI: GPT-4o-mini (2024-07-18)",
        128_000,
        16_384,
        false,
    ),
    built_in_model("openai/gpt-5", "OpenAI: GPT-5", 400_000, 128_000, true),
    built_in_model(
        "openai/gpt-5-codex",
        "OpenAI: GPT-5 Codex",
        400_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5-mini",
        "OpenAI: GPT-5 Mini",
        400_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5-nano",
        "OpenAI: GPT-5 Nano",
        400_000,
        4_096,
        true,
    ),
    built_in_model(
        "openai/gpt-5-pro",
        "OpenAI: GPT-5 Pro",
        400_000,
        128_000,
        true,
    ),
    built_in_model("openai/gpt-5.1", "OpenAI: GPT-5.1", 400_000, 128_000, true),
    built_in_model(
        "openai/gpt-5.1-chat",
        "OpenAI: GPT-5.1 Chat",
        128_000,
        32_000,
        false,
    ),
    built_in_model(
        "openai/gpt-5.1-codex",
        "OpenAI: GPT-5.1-Codex",
        400_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5.1-codex-max",
        "OpenAI: GPT-5.1-Codex-Max",
        400_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5.1-codex-mini",
        "OpenAI: GPT-5.1-Codex-Mini",
        400_000,
        100_000,
        true,
    ),
    built_in_model("openai/gpt-5.2", "OpenAI: GPT-5.2", 400_000, 128_000, true),
    built_in_model(
        "openai/gpt-5.2-chat",
        "OpenAI: GPT-5.2 Chat",
        128_000,
        16_384,
        false,
    ),
    built_in_model(
        "openai/gpt-5.2-codex",
        "OpenAI: GPT-5.2-Codex",
        400_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5.2-pro",
        "OpenAI: GPT-5.2 Pro",
        400_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5.3-chat",
        "OpenAI: GPT-5.3 Chat",
        128_000,
        16_384,
        false,
    ),
    built_in_model(
        "openai/gpt-5.3-codex",
        "OpenAI: GPT-5.3-Codex",
        400_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5.4",
        "OpenAI: GPT-5.4",
        1_050_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5.4-mini",
        "OpenAI: GPT-5.4 Mini",
        400_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5.4-nano",
        "OpenAI: GPT-5.4 Nano",
        400_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5.4-pro",
        "OpenAI: GPT-5.4 Pro",
        1_050_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5.5",
        "OpenAI: GPT-5.5",
        1_050_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5.5-pro",
        "OpenAI: GPT-5.5 Pro",
        1_050_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5.6-luna",
        "OpenAI: GPT-5.6 Luna",
        1_050_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5.6-luna-pro",
        "OpenAI: GPT-5.6 Luna Pro",
        1_050_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5.6-sol",
        "OpenAI: GPT-5.6 Sol",
        1_050_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5.6-sol-pro",
        "OpenAI: GPT-5.6 Sol Pro",
        1_050_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5.6-terra",
        "OpenAI: GPT-5.6 Terra",
        1_050_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-5.6-terra-pro",
        "OpenAI: GPT-5.6 Terra Pro",
        1_050_000,
        128_000,
        true,
    ),
    built_in_model(
        "openai/gpt-audio",
        "OpenAI: GPT Audio",
        128_000,
        16_384,
        false,
    ),
    built_in_model(
        "openai/gpt-audio-mini",
        "OpenAI: GPT Audio Mini",
        128_000,
        16_384,
        false,
    ),
    built_in_model(
        "openai/gpt-chat-latest",
        "OpenAI: GPT Chat Latest",
        400_000,
        128_000,
        false,
    ),
    built_in_model(
        "openai/gpt-oss-120b",
        "OpenAI: gpt-oss-120b",
        131_072,
        4_096,
        true,
    ),
    built_in_model(
        "openai/gpt-oss-20b",
        "OpenAI: gpt-oss-20b",
        131_072,
        4_096,
        true,
    ),
    built_in_model(
        "openai/gpt-oss-20b:free",
        "OpenAI: gpt-oss-20b (free)",
        131_072,
        32_768,
        true,
    ),
    built_in_model(
        "openai/gpt-oss-safeguard-20b",
        "OpenAI: gpt-oss-safeguard-20b",
        131_072,
        65_536,
        true,
    ),
    built_in_model("openai/o1", "OpenAI: o1", 200_000, 100_000, true),
    built_in_model("openai/o3", "OpenAI: o3", 200_000, 100_000, true),
    built_in_model(
        "openai/o3-deep-research",
        "OpenAI: o3 Deep Research",
        200_000,
        100_000,
        true,
    ),
    built_in_model("openai/o3-mini", "OpenAI: o3 Mini", 200_000, 100_000, true),
    built_in_model(
        "openai/o3-mini-high",
        "OpenAI: o3 Mini High",
        200_000,
        100_000,
        true,
    ),
    built_in_model("openai/o3-pro", "OpenAI: o3 Pro", 200_000, 100_000, true),
    built_in_model("openai/o4-mini", "OpenAI: o4 Mini", 200_000, 100_000, true),
    built_in_model(
        "openai/o4-mini-deep-research",
        "OpenAI: o4 Mini Deep Research",
        200_000,
        100_000,
        true,
    ),
    built_in_model(
        "openai/o4-mini-high",
        "OpenAI: o4 Mini High",
        200_000,
        100_000,
        true,
    ),
    built_in_model("openrouter/auto", "Auto Router", 2_000_000, 4_096, true),
    built_in_model(
        "openrouter/free",
        "Free Models Router",
        200_000,
        4_096,
        true,
    ),
    built_in_model(
        "openrouter/fusion",
        "OpenRouter: Fusion",
        1_000_000,
        30_000,
        true,
    ),
    built_in_model(
        "poolside/laguna-m.1",
        "Poolside: Laguna M.1",
        262_144,
        32_768,
        true,
    ),
    built_in_model(
        "poolside/laguna-m.1:free",
        "Poolside: Laguna M.1 (free)",
        262_144,
        32_768,
        true,
    ),
    built_in_model(
        "poolside/laguna-xs-2.1",
        "Poolside: Laguna XS 2.1",
        262_144,
        32_768,
        true,
    ),
    built_in_model(
        "poolside/laguna-xs-2.1:free",
        "Poolside: Laguna XS 2.1 (free)",
        262_144,
        32_768,
        true,
    ),
    built_in_model(
        "qwen/qwen-2.5-72b-instruct",
        "Qwen2.5 72B Instruct",
        32_768,
        16_384,
        false,
    ),
    built_in_model(
        "qwen/qwen-2.5-7b-instruct",
        "Qwen: Qwen2.5 7B Instruct",
        32_768,
        32_768,
        false,
    ),
    built_in_model(
        "qwen/qwen-plus",
        "Qwen: Qwen-Plus",
        1_000_000,
        32_768,
        false,
    ),
    built_in_model(
        "qwen/qwen-plus-2025-07-28",
        "Qwen: Qwen Plus 0728",
        1_000_000,
        32_768,
        false,
    ),
    built_in_model(
        "qwen/qwen-plus-2025-07-28:thinking",
        "Qwen: Qwen Plus 0728 (thinking)",
        1_000_000,
        32_768,
        true,
    ),
    built_in_model("qwen/qwen3-14b", "Qwen: Qwen3 14B", 40_960, 40_960, true),
    built_in_model(
        "qwen/qwen3-235b-a22b",
        "Qwen: Qwen3 235B A22B",
        131_072,
        8_192,
        true,
    ),
    built_in_model(
        "qwen/qwen3-235b-a22b-2507",
        "Qwen: Qwen3 235B A22B Instruct 2507",
        262_144,
        16_384,
        false,
    ),
    built_in_model(
        "qwen/qwen3-235b-a22b-thinking-2507",
        "Qwen: Qwen3 235B A22B Thinking 2507",
        131_072,
        4_096,
        true,
    ),
    built_in_model(
        "qwen/qwen3-30b-a3b",
        "Qwen: Qwen3 30B A3B",
        40_960,
        16_384,
        true,
    ),
    built_in_model(
        "qwen/qwen3-30b-a3b-instruct-2507",
        "Qwen: Qwen3 30B A3B Instruct 2507",
        128_000,
        32_000,
        false,
    ),
    built_in_model(
        "qwen/qwen3-30b-a3b-thinking-2507",
        "Qwen: Qwen3 30B A3B Thinking 2507",
        81_920,
        32_768,
        true,
    ),
    built_in_model("qwen/qwen3-32b", "Qwen: Qwen3 32B", 40_960, 16_384, true),
    built_in_model("qwen/qwen3-8b", "Qwen: Qwen3 8B", 131_072, 8_192, true),
    built_in_model(
        "qwen/qwen3-coder",
        "Qwen: Qwen3 Coder 480B A35B",
        262_144,
        65_536,
        false,
    ),
    built_in_model(
        "qwen/qwen3-coder-30b-a3b-instruct",
        "Qwen: Qwen3 Coder 30B A3B Instruct",
        160_000,
        32_768,
        false,
    ),
    built_in_model(
        "qwen/qwen3-coder-flash",
        "Qwen: Qwen3 Coder Flash",
        1_000_000,
        65_536,
        false,
    ),
    built_in_model(
        "qwen/qwen3-coder-next",
        "Qwen: Qwen3 Coder Next",
        262_144,
        262_144,
        false,
    ),
    built_in_model(
        "qwen/qwen3-coder-plus",
        "Qwen: Qwen3 Coder Plus",
        1_000_000,
        65_536,
        false,
    ),
    built_in_model(
        "qwen/qwen3-coder:free",
        "Qwen: Qwen3 Coder 480B A35B (free)",
        262_000,
        262_000,
        false,
    ),
    built_in_model("qwen/qwen3-max", "Qwen: Qwen3 Max", 262_144, 32_768, false),
    built_in_model(
        "qwen/qwen3-max-thinking",
        "Qwen: Qwen3 Max Thinking",
        262_144,
        32_768,
        true,
    ),
    built_in_model(
        "qwen/qwen3-next-80b-a3b-instruct",
        "Qwen: Qwen3 Next 80B A3B Instruct",
        262_144,
        16_384,
        false,
    ),
    built_in_model(
        "qwen/qwen3-next-80b-a3b-instruct:free",
        "Qwen: Qwen3 Next 80B A3B Instruct (free)",
        262_144,
        4_096,
        false,
    ),
    built_in_model(
        "qwen/qwen3-next-80b-a3b-thinking",
        "Qwen: Qwen3 Next 80B A3B Thinking",
        131_072,
        32_768,
        true,
    ),
    built_in_model(
        "qwen/qwen3-vl-235b-a22b-instruct",
        "Qwen: Qwen3 VL 235B A22B Instruct",
        262_144,
        16_384,
        false,
    ),
    built_in_model(
        "qwen/qwen3-vl-235b-a22b-thinking",
        "Qwen: Qwen3 VL 235B A22B Thinking",
        131_072,
        32_768,
        true,
    ),
    built_in_model(
        "qwen/qwen3-vl-30b-a3b-instruct",
        "Qwen: Qwen3 VL 30B A3B Instruct",
        131_072,
        32_768,
        false,
    ),
    built_in_model(
        "qwen/qwen3-vl-30b-a3b-thinking",
        "Qwen: Qwen3 VL 30B A3B Thinking",
        131_072,
        32_768,
        true,
    ),
    built_in_model(
        "qwen/qwen3-vl-32b-instruct",
        "Qwen: Qwen3 VL 32B Instruct",
        131_072,
        32_768,
        false,
    ),
    built_in_model(
        "qwen/qwen3-vl-8b-instruct",
        "Qwen: Qwen3 VL 8B Instruct",
        131_072,
        32_768,
        false,
    ),
    built_in_model(
        "qwen/qwen3-vl-8b-thinking",
        "Qwen: Qwen3 VL 8B Thinking",
        131_072,
        32_768,
        true,
    ),
    built_in_model(
        "qwen/qwen3.5-122b-a10b",
        "Qwen: Qwen3.5-122B-A10B",
        262_144,
        262_144,
        true,
    ),
    built_in_model(
        "qwen/qwen3.5-27b",
        "Qwen: Qwen3.5-27B",
        262_144,
        65_536,
        true,
    ),
    built_in_model(
        "qwen/qwen3.5-35b-a3b",
        "Qwen: Qwen3.5-35B-A3B",
        262_144,
        81_920,
        true,
    ),
    built_in_model(
        "qwen/qwen3.5-397b-a17b",
        "Qwen: Qwen3.5 397B A17B",
        131_072,
        4_096,
        true,
    ),
    built_in_model(
        "qwen/qwen3.5-9b",
        "Qwen: Qwen3.5-9B",
        262_144,
        262_144,
        true,
    ),
    built_in_model(
        "qwen/qwen3.5-flash-02-23",
        "Qwen: Qwen3.5-Flash",
        1_000_000,
        65_536,
        true,
    ),
    built_in_model(
        "qwen/qwen3.5-plus-02-15",
        "Qwen: Qwen3.5 Plus 2026-02-15",
        1_000_000,
        65_536,
        true,
    ),
    built_in_model(
        "qwen/qwen3.5-plus-20260420",
        "Qwen: Qwen3.5 Plus 2026-04-20",
        1_000_000,
        65_536,
        true,
    ),
    built_in_model(
        "qwen/qwen3.6-27b",
        "Qwen: Qwen3.6 27B",
        262_140,
        262_140,
        true,
    ),
    built_in_model(
        "qwen/qwen3.6-35b-a3b",
        "Qwen: Qwen3.6 35B A3B",
        262_144,
        262_144,
        true,
    ),
    built_in_model(
        "qwen/qwen3.6-flash",
        "Qwen: Qwen3.6 Flash",
        1_000_000,
        65_536,
        true,
    ),
    built_in_model(
        "qwen/qwen3.6-max-preview",
        "Qwen: Qwen3.6 Max Preview",
        262_144,
        65_536,
        true,
    ),
    built_in_model(
        "qwen/qwen3.6-plus",
        "Qwen: Qwen3.6 Plus",
        1_000_000,
        65_536,
        true,
    ),
    built_in_model(
        "qwen/qwen3.7-max",
        "Qwen: Qwen3.7 Max",
        1_000_000,
        65_536,
        true,
    ),
    built_in_model(
        "qwen/qwen3.7-plus",
        "Qwen: Qwen3.7 Plus",
        1_000_000,
        65_536,
        true,
    ),
    built_in_model("rekaai/reka-edge", "Reka Edge", 16_384, 16_384, false),
    built_in_model(
        "relace/relace-search",
        "Relace: Relace Search",
        256_000,
        128_000,
        false,
    ),
    built_in_model(
        "sakana/fugu-ultra",
        "Sakana: Fugu Ultra",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "sao10k/l3.1-euryale-70b",
        "Sao10K: Llama 3.1 Euryale 70B v2.2",
        131_072,
        16_384,
        false,
    ),
    built_in_model(
        "stepfun/step-3.5-flash",
        "StepFun: Step 3.5 Flash",
        262_144,
        65_536,
        true,
    ),
    built_in_model(
        "stepfun/step-3.7-flash",
        "StepFun: Step 3.7 Flash",
        256_000,
        256_000,
        true,
    ),
    built_in_model("tencent/hy3", "Tencent: Hy3", 262_144, 4_096, true),
    built_in_model(
        "tencent/hy3-preview",
        "Tencent: Hy3 preview",
        262_144,
        4_096,
        true,
    ),
    built_in_model(
        "tencent/hy3:free",
        "Tencent: Hy3 (free)",
        262_144,
        262_144,
        true,
    ),
    built_in_model(
        "thedrummer/unslopnemo-12b",
        "TheDrummer: UnslopNemo 12B",
        32_768,
        32_768,
        false,
    ),
    built_in_model(
        "thinkingmachines/inkling",
        "Thinking Machines: Inkling",
        1_048_576,
        1_048_576,
        true,
    ),
    built_in_model(
        "upstage/solar-pro-3",
        "Upstage: Solar Pro 3",
        128_000,
        4_096,
        true,
    ),
    built_in_model("x-ai/grok-4.20", "xAI: Grok 4.20", 2_000_000, 4_096, true),
    built_in_model("x-ai/grok-4.3", "xAI: Grok 4.3", 1_000_000, 4_096, true),
    built_in_model("x-ai/grok-4.5", "xAI: Grok 4.5", 500_000, 4_096, true),
    built_in_model(
        "x-ai/grok-build-0.1",
        "xAI: Grok Build 0.1",
        256_000,
        4_096,
        true,
    ),
    built_in_model("xiaomi/mimo-v2.5", "Xiaomi: MiMo-V2.5", 32_000, 4_096, true),
    built_in_model(
        "xiaomi/mimo-v2.5-pro",
        "Xiaomi: MiMo-V2.5-Pro",
        1_048_576,
        131_072,
        true,
    ),
    built_in_model("z-ai/glm-4.5", "Z.ai: GLM 4.5", 131_072, 98_304, true),
    built_in_model(
        "z-ai/glm-4.5-air",
        "Z.ai: GLM 4.5 Air",
        131_072,
        98_304,
        true,
    ),
    built_in_model("z-ai/glm-4.5v", "Z.ai: GLM 4.5V", 65_536, 16_384, true),
    built_in_model("z-ai/glm-4.6", "Z.ai: GLM 4.6", 202_752, 131_072, true),
    built_in_model("z-ai/glm-4.6v", "Z.ai: GLM 4.6V", 131_072, 32_768, true),
    built_in_model("z-ai/glm-4.7", "Z.ai: GLM 4.7", 202_752, 131_072, true),
    built_in_model(
        "z-ai/glm-4.7-flash",
        "Z.ai: GLM 4.7 Flash",
        202_752,
        16_384,
        true,
    ),
    built_in_model("z-ai/glm-5", "Z.ai: GLM 5", 202_752, 4_096, true),
    built_in_model(
        "z-ai/glm-5-turbo",
        "Z.ai: GLM 5 Turbo",
        262_144,
        131_072,
        true,
    ),
    built_in_model("z-ai/glm-5.1", "Z.ai: GLM 5.1", 200_000, 128_000, true),
    built_in_model("z-ai/glm-5.2", "Z.ai: GLM 5.2", 1_024_000, 128_000, true),
    built_in_model(
        "z-ai/glm-5v-turbo",
        "Z.ai: GLM 5V Turbo",
        202_752,
        131_072,
        true,
    ),
    built_in_model(
        "~anthropic/claude-fable-latest",
        "Anthropic: Claude Fable Latest",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "~anthropic/claude-haiku-latest",
        "Anthropic Claude Haiku Latest",
        200_000,
        64_000,
        true,
    ),
    built_in_model(
        "~anthropic/claude-opus-latest",
        "Anthropic: Claude Opus Latest",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "~anthropic/claude-sonnet-latest",
        "Anthropic Claude Sonnet Latest",
        1_000_000,
        128_000,
        true,
    ),
    built_in_model(
        "~google/gemini-flash-latest",
        "Google Gemini Flash Latest",
        1_048_576,
        65_536,
        true,
    ),
    built_in_model(
        "~google/gemini-pro-latest",
        "Google Gemini Pro Latest",
        1_048_576,
        65_536,
        true,
    ),
    built_in_model(
        "~moonshotai/kimi-latest",
        "MoonshotAI Kimi Latest",
        262_144,
        262_144,
        true,
    ),
    built_in_model(
        "~openai/gpt-latest",
        "OpenAI GPT Latest",
        1_050_000,
        128_000,
        true,
    ),
    built_in_model(
        "~openai/gpt-mini-latest",
        "OpenAI GPT Mini Latest",
        400_000,
        128_000,
        true,
    ),
    built_in_model(
        "~x-ai/grok-latest",
        "xAI: Grok Latest",
        500_000,
        4_096,
        true,
    ),
];

/// xAI API models (pi reference: xai.models.ts).
const XAI_MODELS: &[BuiltInModelDescriptor] = &[
    built_in_model("grok-3", "Grok 3", 131_072, 8_192, false),
    built_in_model("grok-3-fast", "Grok 3 Fast", 131_072, 8_192, false),
    built_in_model(
        "grok-4.20-0309-non-reasoning",
        "Grok 4.20 (Non-Reasoning)",
        1_000_000,
        30_000,
        false,
    ),
    built_in_model(
        "grok-4.20-0309-reasoning",
        "Grok 4.20 (Reasoning)",
        1_000_000,
        30_000,
        true,
    ),
    built_in_model("grok-4.3", "Grok 4.3", 1_000_000, 30_000, true),
    built_in_model("grok-4.5", "Grok 4.5", 500_000, 500_000, true),
    built_in_model("grok-build-0.1", "Grok Build 0.1", 256_000, 256_000, true),
    built_in_model("grok-code-fast-1", "Grok Code Fast 1", 32_768, 8_192, false),
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
        effective_context_window_percent: None,
        auto_compact_token_limit: None,
    }
}

const fn chatgpt_model(
    id: &'static str,
    display_name: &'static str,
    context_window_tokens: u64,
) -> BuiltInModelDescriptor {
    let (effective_context_window_percent, auto_compact_token_limit) =
        chatgpt_context_policy(Some(context_window_tokens));
    BuiltInModelDescriptor {
        id,
        display_name,
        context_window_tokens: Some(context_window_tokens),
        max_output_tokens: Some(128_000),
        supports_tools: Some(true),
        supports_reasoning: Some(true),
        effective_context_window_percent,
        auto_compact_token_limit,
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
                                effective_context_window_percent: model
                                    .effective_context_window_percent,
                                auto_compact_token_limit: model.auto_compact_token_limit,
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
        let generated_by_models_refresh = root.get("generated_by").and_then(Value::as_str)
            == Some(EULER_MODELS_REFRESH_GENERATOR);

        for (provider_key, provider_value) in providers {
            if generated_by_models_refresh && provider_key == CHATGPT_PROVIDER_ID {
                warnings.push(
                    "ignored stale ChatGPT metadata generated by `euler models refresh`; run refresh again to remove it"
                        .to_owned(),
                );
                continue;
            }
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
        let (effective_context_window_percent, auto_compact_token_limit) =
            if provider_key == CHATGPT_PROVIDER_ID {
                chatgpt_context_policy(context_window_tokens)
            } else {
                (None, None)
            };
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
                effective_context_window_percent,
                auto_compact_token_limit,
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

pub fn supported_reasoning_efforts(provider: &str, model: &str) -> &'static [ReasoningEffort] {
    match (provider, model) {
        (
            CHATGPT_PROVIDER_ID | OPENAI_PROVIDER_ID,
            "gpt-5.6-luna" | "gpt-5.6-sol" | "gpt-5.6-terra",
        ) => MAX_REASONING_EFFORTS,
        (OPENROUTER_PROVIDER_ID, "moonshotai/kimi-k3") => MAX_ONLY_REASONING_EFFORTS,
        (OPENROUTER_PROVIDER_ID, "thinkingmachines/inkling") => INKLING_REASONING_EFFORTS,
        _ => STANDARD_REASONING_EFFORTS,
    }
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
    let known_model = provider_descriptor(provider).ok().is_some_and(|provider| {
        provider
            .models
            .iter()
            .any(|candidate| candidate.id == model)
    });
    if !known_model {
        return requested;
    }

    let supported = supported_reasoning_efforts(provider, model);
    if supported.contains(&requested) {
        requested
    } else {
        supported
            .last()
            .copied()
            .expect("reasoning effort catalog must not be empty")
    }
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
        assert_eq!(XAI_MODELS.len(), 8);
        let default = XAI_MODELS
            .iter()
            .find(|model| model.id == DEFAULT_XAI_MODEL)
            .expect("default xai model listed");
        assert_eq!(default.display_name, "Grok 4.3");
        assert_eq!(default.supports_reasoning, Some(true));
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
}
