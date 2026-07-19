//! Provider abstraction plus offline fixture and ChatGPT providers.
#![cfg_attr(test, allow(clippy::too_many_lines))] // unit-test exemption for inline test modules

pub mod anthropic;
pub mod auth;
pub mod catalog;
mod chat_completions;
mod chat_completions_provider;
pub mod chatgpt;
mod chatgpt_device;
mod chatgpt_websocket;
pub mod custom_provider;
pub mod openai;
pub mod openrouter;
pub mod provider_config;
pub mod sse;
pub mod xai;

#[cfg(test)]
mod conformance_tests;
#[cfg(test)]
mod custom_provider_test;
#[cfg(test)]
mod provider_config_test;
#[cfg(test)]
mod scripted_provider_test;
#[cfg(test)]
mod test_support;

use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRequest {
    pub model: String,
    pub instructions: String,
    pub input: Vec<ModelInputItem>,
    pub tools: Vec<ToolDefinition>,
    pub reasoning_effort: ReasoningEffort,
    pub max_output_tokens: Option<u64>,
}

impl ModelRequest {
    pub fn for_target(mut self, provider: &str, model: &str) -> Self {
        self.model = model.to_owned();
        self.input = input_for_target(&self.input, provider, model);
        self
    }

    pub fn prompt_text(&self) -> String {
        self.input
            .iter()
            .map(|item| match item {
                ModelInputItem::Message { role, content } => {
                    format!("{}: {content}", role.as_str())
                }
                ModelInputItem::ToolCall {
                    call_id,
                    name,
                    arguments,
                } => format!("tool.call {call_id} {name}: {arguments}"),
                ModelInputItem::ToolOutput {
                    call_id,
                    name,
                    ok,
                    output,
                    error,
                    exit_code,
                } => {
                    let prefix = if *ok { "" } else { "[tool failed] " };
                    let content = output.as_deref().or(error.as_deref()).unwrap_or_default();
                    let code = exit_code
                        .map(|code| format!(" exit_code={code}"))
                        .unwrap_or_default();
                    format!("tool.output {call_id} {name}:{code} {prefix}{content}")
                }
                ModelInputItem::Reasoning {
                    provider,
                    model,
                    fidelity,
                    content,
                    artifact,
                } => {
                    let suffix = artifact
                        .as_ref()
                        .map(|_| " artifact=opaque")
                        .unwrap_or_default();
                    format!(
                        "reasoning.{}/{}.{}:{suffix} {content}",
                        provider,
                        model,
                        fidelity.as_str()
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReasoningEffort {
    XSmall,
    Small,
    #[default]
    Medium,
    Large,
    XLarge,
    Max,
}

impl ReasoningEffort {
    pub const ALL: [Self; 6] = [
        Self::XSmall,
        Self::Small,
        Self::Medium,
        Self::Large,
        Self::XLarge,
        Self::Max,
    ];

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "xsmall" => Some(Self::XSmall),
            "small" => Some(Self::Small),
            "medium" => Some(Self::Medium),
            "large" => Some(Self::Large),
            "xlarge" => Some(Self::XLarge),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::XSmall => "xsmall",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
            Self::XLarge => "xlarge",
            Self::Max => "max",
        }
    }

    pub(crate) fn compat_level(self) -> &'static str {
        match self {
            Self::XSmall => "minimal",
            Self::Small => "low",
            Self::Medium => "medium",
            Self::Large => "high",
            Self::XLarge => "xhigh",
            Self::Max => "max",
        }
    }
}

pub fn input_for_target(
    input: &[ModelInputItem],
    target_provider: &str,
    target_model: &str,
) -> Vec<ModelInputItem> {
    input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::Reasoning {
                provider,
                model,
                fidelity,
                content,
                artifact: _,
            } if provider != target_provider || model != target_model => match fidelity {
                ReasoningFidelity::Raw | ReasoningFidelity::Summary if !content.is_empty() => {
                    Some(ModelInputItem::Message {
                        role: ModelRole::Assistant,
                        content: content.clone(),
                    })
                }
                _ => None,
            },
            item => Some(item.clone()),
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelInputItem {
    Message {
        role: ModelRole,
        content: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: Value,
    },
    ToolOutput {
        call_id: String,
        name: String,
        ok: bool,
        output: Option<String>,
        error: Option<String>,
        exit_code: Option<i64>,
    },
    Reasoning {
        provider: String,
        model: String,
        fidelity: ReasoningFidelity,
        content: String,
        artifact: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRole {
    User,
    Assistant,
}

impl ModelRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReasoningFidelity {
    Raw,
    Summary,
    Opaque,
}

impl ReasoningFidelity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Summary => "summary",
            Self::Opaque => "opaque",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ReasoningChunk {
    pub fidelity: ReasoningFidelity,
    pub content: String,
    pub artifact: Option<String>,
}

impl ReasoningChunk {
    pub fn raw(content: impl Into<String>) -> Self {
        Self {
            fidelity: ReasoningFidelity::Raw,
            content: content.into(),
            artifact: None,
        }
    }

    pub fn raw_artifact(content: impl Into<String>, artifact: impl Into<String>) -> Self {
        Self {
            fidelity: ReasoningFidelity::Raw,
            content: content.into(),
            artifact: Some(artifact.into()),
        }
    }

    pub fn summary(content: impl Into<String>) -> Self {
        Self {
            fidelity: ReasoningFidelity::Summary,
            content: content.into(),
            artifact: None,
        }
    }

    pub fn summary_artifact(artifact: impl Into<String>) -> Self {
        Self {
            fidelity: ReasoningFidelity::Summary,
            content: String::new(),
            artifact: Some(artifact.into()),
        }
    }

    pub fn opaque_artifact(artifact: impl Into<String>) -> Self {
        Self {
            fidelity: ReasoningFidelity::Opaque,
            content: String::new(),
            artifact: Some(artifact.into()),
        }
    }
}

impl std::fmt::Debug for ReasoningChunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReasoningChunk")
            .field("fidelity", &self.fidelity)
            .field("content", &self.content)
            .field(
                "artifact",
                &self.artifact.as_ref().map(|_| "[opaque artifact]"),
            )
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelStreamEvent {
    TextDelta(String),
    ReasoningDelta(ReasoningChunk),
    ToolCall(ToolCall),
    Finished {
        stop_reason: StopReason,
        usage: Option<Usage>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StopReason {
    Completed,
    ToolUse,
    MaxTokens,
    Refusal,
    Error,
}

impl StopReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::ToolUse => "tool_use",
            Self::MaxTokens => "max_tokens",
            Self::Refusal => "refusal",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Usage {
    /// Total input tokens reported by the provider. For OpenAI-compatible
    /// APIs this includes cached input; callers that price usage must charge
    /// `cached_tokens` at the cache-read rate and only the remainder at the
    /// ordinary input rate. Anthropic reports uncached input separately.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: Option<u64>,
    /// Provider-reported prompt-cache creation tokens. Anthropic exposes this
    /// separately from ordinary and cache-read input; most providers omit it.
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderErrorCategory {
    Auth,
    Transport,
    RateLimit,
    Rejected,
    StreamTruncation,
}

impl ProviderErrorCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Transport => "transport",
            Self::RateLimit => "rate_limit",
            Self::Rejected => "rejected",
            Self::StreamTruncation => "stream_truncation",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderError {
    category: ProviderErrorCategory,
    message: String,
}

impl ProviderError {
    pub fn new(category: ProviderErrorCategory, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
        }
    }

    pub fn auth(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorCategory::Auth, message)
    }

    pub fn transport(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorCategory::Transport, message)
    }

    pub fn rate_limit(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorCategory::RateLimit, message)
    }

    pub fn rejected(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorCategory::Rejected, message)
    }

    pub fn stream_truncation(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorCategory::StreamTruncation, message)
    }

    pub fn category(&self) -> ProviderErrorCategory {
        self.category
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

pub type ProviderStream = Box<dyn Iterator<Item = Result<ModelStreamEvent, ProviderError>> + Send>;

/// Observer for secret values a provider resolves at request time (custom
/// provider `$ENV` / `!command` / literal api_key and header values). The
/// session installs a sink that registers each value with its redactor so
/// the value is secret-tainted from the moment it exists (secrets contract,
/// "any value resolved through this contract"). Must be `Send + Sync`:
/// parallel reviewer workers invoke providers off the session thread.
pub type ResolvedSecretSink = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// `Sync` is required so a `ProviderSet` can be shared across the parallel
/// reviewer fan-out's worker threads (multi-agent contract v0.2). Providers
/// are stateless request adapters; scripted/test providers use `Mutex` for
/// their queues.
pub trait ModelProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn validate_auth(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    /// Provider-native reasoning effort selected by the adapter for this model.
    ///
    /// This is provenance metadata for `model.call`, not a core policy knob.
    /// Core must not interpret it or assume it is a request-selectable level.
    fn reasoning_effort(&self, _model: &str) -> Option<&str> {
        None
    }
    /// Install the host's resolved-secret observer. Default no-op: only
    /// providers that resolve secrets at request time (custom providers)
    /// have anything to report; built-in providers read pre-seeded env vars
    /// and the auth file, which the host registers directly.
    fn set_resolved_secret_sink(&self, _sink: ResolvedSecretSink) {}
    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError>;
}

impl ModelProvider for Box<dyn ModelProvider> {
    fn name(&self) -> &'static str {
        self.as_ref().name()
    }

    fn validate_auth(&self) -> Result<(), ProviderError> {
        self.as_ref().validate_auth()
    }

    fn reasoning_effort(&self, model: &str) -> Option<&str> {
        self.as_ref().reasoning_effort(model)
    }

    fn set_resolved_secret_sink(&self, sink: ResolvedSecretSink) {
        self.as_ref().set_resolved_secret_sink(sink);
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        self.as_ref().invoke(request)
    }
}

pub struct ProviderSet {
    providers: BTreeMap<String, Box<dyn ModelProvider>>,
    model_catalog: catalog::MergedModelCatalog,
}

impl Default for ProviderSet {
    fn default() -> Self {
        Self {
            providers: BTreeMap::new(),
            model_catalog: catalog::MergedModelCatalog::built_in(),
        }
    }
}

impl ProviderSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn single<P>(provider: P) -> Self
    where
        P: ModelProvider + 'static,
    {
        let mut set = Self::new();
        set.insert(provider);
        set
    }

    pub fn single_named<P>(provider_id: impl Into<String>, provider: P) -> Self
    where
        P: ModelProvider + 'static,
    {
        let mut set = Self::new();
        set.insert_named(provider_id, provider);
        set
    }

    pub fn with_model_catalog(mut self, catalog: catalog::MergedModelCatalog) -> Self {
        self.model_catalog = catalog;
        self
    }

    pub fn set_model_catalog(&mut self, catalog: catalog::MergedModelCatalog) {
        self.model_catalog = catalog;
    }

    pub fn insert<P>(&mut self, provider: P) -> bool
    where
        P: ModelProvider + 'static,
    {
        let name = provider.name().to_owned();
        self.providers.insert(name, Box::new(provider)).is_some()
    }

    pub fn insert_named<P>(&mut self, provider_id: impl Into<String>, provider: P) -> bool
    where
        P: ModelProvider + 'static,
    {
        self.providers
            .insert(provider_id.into(), Box::new(provider))
            .is_some()
    }

    pub fn contains(&self, provider: &str) -> bool {
        self.providers.contains_key(provider)
    }

    /// Configured AND authenticated: `validate_auth` succeeds today. This is
    /// a live credential check (env var / token file presence, not a network
    /// call) so it is cheap enough to run when populating a picker.
    pub fn is_authenticated(&self, provider: &str) -> bool {
        self.providers
            .get(provider)
            .is_some_and(|provider| provider.validate_auth().is_ok())
    }

    /// Every configured provider id whose `validate_auth` succeeds today —
    /// the same predicate as [`Self::is_authenticated`], enumerated so
    /// callers that lose access to the set (e.g. while a session is checked
    /// out onto a worker thread) can keep a last-known snapshot.
    pub fn authenticated_provider_ids(&self) -> std::collections::BTreeSet<String> {
        self.providers
            .iter()
            .filter(|(_, provider)| provider.validate_auth().is_ok())
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub fn reasoning_effort(&self, provider: &str, model: &str) -> Option<&str> {
        self.providers
            .get(provider)
            .and_then(|provider| provider.reasoning_effort(model))
    }

    /// Normalize a carried user-selectable effort against the destination
    /// model's provider catalog. Targets outside the built-in catalog are left
    /// unchanged; switch validation reports unconfigured providers separately.
    pub fn clamp_reasoning_effort(
        &self,
        provider: &str,
        model: &str,
        requested: ReasoningEffort,
    ) -> ReasoningEffort {
        if self.providers.contains_key(provider) {
            self.model_catalog
                .clamp_reasoning_effort(provider, model, requested)
        } else {
            requested
        }
    }

    pub fn invoke(
        &self,
        provider: &str,
        request: ModelRequest,
    ) -> Result<ProviderStream, ProviderError> {
        let Some(provider) = self.providers.get(provider) else {
            return Err(ProviderError::rejected(format!(
                "provider is not configured: {provider}"
            )));
        };
        provider.invoke(request)
    }

    /// Install `sink` on every configured provider so request-time secret
    /// resolution reports each value to the host (see [`ResolvedSecretSink`]).
    pub fn install_resolved_secret_sink(&self, sink: ResolvedSecretSink) {
        for provider in self.providers.values() {
            provider.set_resolved_secret_sink(std::sync::Arc::clone(&sink));
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct EchoProvider;

impl ModelProvider for EchoProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        let usage = synthetic_usage(&request.prompt_text(), "");
        let events = vec![
            Ok(ModelStreamEvent::TextDelta(request.prompt_text())),
            Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: Some(usage),
            }),
        ];
        Ok(Box::new(events.into_iter()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FixtureResponse {
    Assistant(String),
    ReasoningThenAssistant { reasoning: String, content: String },
    ToolCalls(Vec<ToolCall>),
    Stream(Vec<ScriptedStreamStep>),
}

#[derive(Debug)]
pub struct ScriptedProvider {
    responses: std::sync::Mutex<VecDeque<FixtureResponse>>,
    reasoning_effort: Option<String>,
}

impl ScriptedProvider {
    pub fn new(responses: Vec<FixtureResponse>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses.into()),
            reasoning_effort: None,
        }
    }

    pub fn with_reasoning_effort(mut self, reasoning_effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(reasoning_effort.into());
        self
    }
}

impl ModelProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn reasoning_effort(&self, _model: &str) -> Option<&str> {
        self.reasoning_effort.as_deref()
    }

    fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        let Some(response) = self
            .responses
            .lock()
            .expect("scripted provider queue")
            .pop_front()
        else {
            return Err(ProviderError::transport("scripted provider exhausted"));
        };
        let events = match response {
            FixtureResponse::Assistant(content) => {
                vec![
                    Ok(ModelStreamEvent::TextDelta(content.clone())),
                    Ok(ModelStreamEvent::Finished {
                        stop_reason: StopReason::Completed,
                        usage: Some(synthetic_usage("", &content)),
                    }),
                ]
            }
            FixtureResponse::ReasoningThenAssistant { reasoning, content } => vec![
                Ok(ModelStreamEvent::ReasoningDelta(ReasoningChunk::summary(
                    reasoning.clone(),
                ))),
                Ok(ModelStreamEvent::TextDelta(content.clone())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: Some(synthetic_usage(&reasoning, &content)),
                }),
            ],
            FixtureResponse::ToolCalls(calls) => calls
                .into_iter()
                .map(|call| Ok(ModelStreamEvent::ToolCall(call)))
                .chain(std::iter::once(Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::ToolUse,
                    usage: Some(synthetic_usage("", "")),
                })))
                .collect(),
            FixtureResponse::Stream(steps) => return Ok(Box::new(ScriptedStream::new(steps))),
        };
        Ok(Box::new(events.into_iter()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptedStreamStep {
    Event(ModelStreamEvent),
    SleepMs(u64),
}

#[derive(Debug)]
struct ScriptedStream {
    steps: VecDeque<ScriptedStreamStep>,
}

impl ScriptedStream {
    fn new(steps: Vec<ScriptedStreamStep>) -> Self {
        Self {
            steps: steps.into(),
        }
    }
}

impl Iterator for ScriptedStream {
    type Item = Result<ModelStreamEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.steps.pop_front()? {
                ScriptedStreamStep::Event(event) => return Some(Ok(event)),
                ScriptedStreamStep::SleepMs(milliseconds) => {
                    std::thread::sleep(Duration::from_millis(milliseconds));
                }
            }
        }
    }
}

fn synthetic_usage(input: &str, output: &str) -> Usage {
    Usage {
        input_tokens: input.split_whitespace().count() as u64,
        output_tokens: output.split_whitespace().count() as u64,
        cached_tokens: Some(0),
        cache_write_tokens: Some(0),
        reasoning_tokens: Some(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_text_marks_failed_tool_outputs() {
        let request = ModelRequest {
            model: "fixture".to_owned(),
            instructions: String::new(),
            input: vec![ModelInputItem::ToolOutput {
                call_id: "call-1".to_owned(),
                name: "run_shell".to_owned(),
                ok: false,
                output: None,
                error: Some("permission denied".to_owned()),
                exit_code: None,
            }],
            tools: Vec::new(),
            reasoning_effort: crate::ReasoningEffort::Medium,
            max_output_tokens: None,
        };

        assert_eq!(
            request.prompt_text(),
            "tool.output call-1 run_shell: [tool failed] permission denied"
        );
    }

    #[test]
    fn target_input_preserves_same_target_reasoning_artifacts_only() {
        let input = vec![
            ModelInputItem::Reasoning {
                provider: "anthropic".to_owned(),
                model: "claude".to_owned(),
                fidelity: ReasoningFidelity::Summary,
                content: "same target".to_owned(),
                artifact: Some("same-signature".to_owned()),
            },
            ModelInputItem::Reasoning {
                provider: "anthropic".to_owned(),
                model: "other-claude".to_owned(),
                fidelity: ReasoningFidelity::Summary,
                content: "other model".to_owned(),
                artifact: Some("must-drop".to_owned()),
            },
            ModelInputItem::Reasoning {
                provider: "anthropic".to_owned(),
                model: "other-claude".to_owned(),
                fidelity: ReasoningFidelity::Opaque,
                content: String::new(),
                artifact: Some("must-drop-opaque".to_owned()),
            },
        ];

        let filtered = input_for_target(&input, "anthropic", "claude");

        assert_eq!(filtered.len(), 2);
        assert!(matches!(
            &filtered[0],
            ModelInputItem::Reasoning {
                artifact: Some(artifact),
                ..
            } if artifact == "same-signature"
        ));
        assert!(matches!(
            &filtered[1],
            ModelInputItem::Message { content, .. } if content == "other model"
        ));
        assert!(!format!("{filtered:?}").contains("must-drop"));
    }

    #[test]
    fn provider_set_insert_reports_replacement() {
        let mut providers = ProviderSet::new();

        assert!(!providers.insert(EchoProvider));
        assert!(providers.insert(EchoProvider));
        assert!(providers.contains("fixture"));
    }
}
