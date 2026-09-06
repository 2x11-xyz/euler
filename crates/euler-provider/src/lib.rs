//! Provider abstraction shared by the built-in and custom model adapters.
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
mod liveness;
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
use std::sync::Arc;
use std::time::Duration;

/// Version of the provider client implementation linked into this build.
/// Runtime provenance freezes this compile-time value into `session.start`.
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) use liveness::{observed_http_agent, TransportReader};
pub use liveness::{
    CancellationCheck, ProviderAttemptEvent, ProviderAttemptObserver, ProviderAttemptOutcome,
    ProviderAttemptSummary, ProviderLivenessConfig, ProviderTimeoutStage,
    ProviderTransportObserver,
};

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
                ModelInputItem::ProjectContext { rendered } => rendered.clone(),
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
    /// Core-framed repository project context (ADR 0017). The rendered bytes
    /// are produced once by core framing; adapters may wrap them in
    /// provider-specific role or envelope data but must not trim, normalize,
    /// combine, reorder, or silently omit them. An adapter that cannot
    /// represent this item must fail before dispatch.
    ProjectContext {
        rendered: String,
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

impl ModelStreamEvent {
    /// Whether this provider-neutral event proves semantic output that must
    /// advance inactivity state and suppress automatic replay.
    ///
    /// Provider-opaque reasoning artifacts are intentionally excluded: only
    /// their owning adapter may interpret them, and they are not visible model
    /// progress. Empty deltas likewise carry no semantic output.
    pub fn is_provider_neutral_progress(&self) -> bool {
        match self {
            Self::TextDelta(delta) => !delta.is_empty(),
            Self::ReasoningDelta(chunk) => !chunk.content.is_empty(),
            Self::ToolCall(_) | Self::Finished { .. } => true,
        }
    }
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
    /// Total input tokens consumed by the request, including every cache-read
    /// and cache-write bucket. Adapters normalize provider-native counters to
    /// this common total so context accounting never depends on a provider id.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Ordinary input tokens after removing every cache bucket. Pricing is
    /// available only when this and the three cache counters form a disjoint
    /// breakdown whose checked sum equals `input_tokens`.
    pub uncached_input_tokens: Option<u64>,
    /// Input tokens served from a prompt cache.
    pub cached_tokens: Option<u64>,
    /// Prompt-cache creation tokens with the provider's short-lived TTL.
    pub cache_write_5m_tokens: Option<u64>,
    /// Prompt-cache creation tokens with a one-hour TTL. This is disjoint from
    /// `cache_write_5m_tokens`, not a subset of it.
    pub cache_write_1h_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    /// Normalize APIs whose top-level input count includes cache reads and
    /// writes. Inconsistent subtotals preserve the provider's total for
    /// context accounting but deliberately leave the charge buckets absent.
    pub(crate) fn from_inclusive_input(
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: Option<u64>,
        cache_write_5m_tokens: Option<u64>,
        cache_write_1h_tokens: Option<u64>,
        reasoning_tokens: Option<u64>,
    ) -> Self {
        let uncached_input_tokens = cache_read_tokens
            .zip(cache_write_5m_tokens)
            .zip(cache_write_1h_tokens)
            .and_then(|((read, short_write), long_write)| {
                read.checked_add(short_write)?.checked_add(long_write)
            })
            .and_then(|cached| input_tokens.checked_sub(cached));
        let valid = uncached_input_tokens.is_some();
        Self {
            input_tokens,
            output_tokens,
            uncached_input_tokens,
            cached_tokens: valid.then_some(cache_read_tokens).flatten(),
            cache_write_5m_tokens: valid.then_some(cache_write_5m_tokens).flatten(),
            cache_write_1h_tokens: valid.then_some(cache_write_1h_tokens).flatten(),
            reasoning_tokens,
        }
    }
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
    request_outcome_unknown: bool,
    timeout_stage: Option<ProviderTimeoutStage>,
    attempt_id: Option<String>,
}

impl ProviderError {
    pub fn new(category: ProviderErrorCategory, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
            request_outcome_unknown: false,
            timeout_stage: None,
            attempt_id: None,
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

    pub fn timeout(stage: ProviderTimeoutStage, limit: Duration) -> Self {
        let mut error = Self::new(
            ProviderErrorCategory::Transport,
            format!(
                "provider inactivity timeout at {} after {} ms",
                stage.as_str(),
                limit.as_millis()
            ),
        );
        error.timeout_stage = Some(stage);
        error
    }

    pub fn rejected(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorCategory::Rejected, message)
    }

    pub fn stream_truncation(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorCategory::StreamTruncation, message)
    }

    /// The detached provider request thread panicked after dispatch may have
    /// begun. Callers must not interpret this as an ordinary provider
    /// rejection or replay the request: the remote outcome is unknown.
    pub fn request_worker_panicked() -> Self {
        let mut error = Self::stream_truncation("provider request worker panicked");
        error.request_outcome_unknown = true;
        error
    }

    pub fn category(&self) -> ProviderErrorCategory {
        self.category
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn request_outcome_unknown(&self) -> bool {
        self.request_outcome_unknown
    }

    pub fn timeout_stage(&self) -> Option<ProviderTimeoutStage> {
        self.timeout_stage
    }

    pub fn attempt_id(&self) -> Option<&str> {
        self.attempt_id.as_deref()
    }

    fn with_attempt_id(mut self, attempt_id: &str) -> Self {
        self.attempt_id = Some(attempt_id.to_owned());
        self
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
    /// Invoke with a transport observer used by the host's liveness boundary.
    ///
    /// Providers that can see raw reads should override this and call
    /// [`ProviderTransportObserver::bytes_received`] whenever bytes or a
    /// protocol heartbeat arrive. The default preserves compatibility and
    /// still receives header and semantic inactivity enforcement from the
    /// host; a semantic event also proves that response bytes arrived.
    fn invoke_observed(
        &self,
        request: ModelRequest,
        _observer: ProviderTransportObserver,
    ) -> Result<ProviderStream, ProviderError> {
        self.invoke(request)
    }
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

    fn invoke_observed(
        &self,
        request: ModelRequest,
        observer: ProviderTransportObserver,
    ) -> Result<ProviderStream, ProviderError> {
        self.as_ref().invoke_observed(request, observer)
    }
}

#[derive(Clone)]
pub struct ProviderSet {
    providers: BTreeMap<String, Arc<dyn ModelProvider>>,
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

    pub fn resolved_model_cost(
        &self,
        provider: &str,
        model: &str,
    ) -> Option<catalog::ResolvedModelCost> {
        self.model_catalog.resolved_model_cost(provider, model)
    }

    pub fn insert<P>(&mut self, provider: P) -> bool
    where
        P: ModelProvider + 'static,
    {
        let name = provider.name().to_owned();
        self.providers.insert(name, Arc::new(provider)).is_some()
    }

    pub fn insert_named<P>(&mut self, provider_id: impl Into<String>, provider: P) -> bool
    where
        P: ModelProvider + 'static,
    {
        self.providers
            .insert(provider_id.into(), Arc::new(provider))
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

    /// Drive one physical provider attempt behind cancellation and inactivity
    /// boundaries. Raw transport observations stay inside this wrapper; the
    /// returned iterator yields only semantic [`ModelStreamEvent`] values.
    pub fn invoke_interruptibly(
        &self,
        provider: &str,
        request: ModelRequest,
        cancellation: CancellationCheck,
        liveness: ProviderLivenessConfig,
        attempt_observer: ProviderAttemptObserver,
    ) -> Result<ProviderStream, ProviderError> {
        let Some(provider) = self.providers.get(provider).cloned() else {
            return Err(ProviderError::rejected(format!(
                "provider is not configured: {provider}"
            )));
        };
        liveness::invoke_interruptibly(provider, request, cancellation, liveness, attempt_observer)
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
    Usage::from_inclusive_input(
        input.split_whitespace().count() as u64,
        output.split_whitespace().count() as u64,
        Some(0),
        Some(0),
        Some(0),
        Some(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::time::Instant;

    fn request() -> ModelRequest {
        ModelRequest {
            model: "fixture".to_owned(),
            instructions: String::new(),
            input: Vec::new(),
            tools: Vec::new(),
            reasoning_effort: ReasoningEffort::Medium,
            max_output_tokens: None,
        }
    }

    fn liveness(
        header_ms: u64,
        first_byte_ms: u64,
        semantic_idle_ms: u64,
    ) -> ProviderLivenessConfig {
        ProviderLivenessConfig {
            response_header_timeout: Duration::from_millis(header_ms),
            first_byte_timeout: Duration::from_millis(first_byte_ms),
            semantic_idle_timeout: Duration::from_millis(semantic_idle_ms),
        }
    }

    fn attempt_events() -> (
        ProviderAttemptObserver,
        Arc<Mutex<Vec<ProviderAttemptEvent>>>,
    ) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&events);
        (
            ProviderAttemptObserver::new(move |event| {
                observed.lock().expect("attempt events").push(event);
            }),
            events,
        )
    }

    #[derive(Clone, Copy, Debug)]
    struct DelayedInvokeProvider {
        delay: Duration,
    }

    impl ModelProvider for DelayedInvokeProvider {
        fn name(&self) -> &'static str {
            "delayed"
        }

        fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
            std::thread::sleep(self.delay);
            Ok(Box::new(std::iter::empty()))
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct DelayedFirstEventProvider {
        delay: Duration,
    }

    impl ModelProvider for DelayedFirstEventProvider {
        fn name(&self) -> &'static str {
            "delayed"
        }

        fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
            Ok(Box::new(DelayedFirstEventStream {
                delay: self.delay,
                yielded: false,
            }))
        }
    }

    struct DelayedFirstEventStream {
        delay: Duration,
        yielded: bool,
    }

    impl Iterator for DelayedFirstEventStream {
        type Item = Result<ModelStreamEvent, ProviderError>;

        fn next(&mut self) -> Option<Self::Item> {
            if self.yielded {
                return None;
            }
            self.yielded = true;
            std::thread::sleep(self.delay);
            Some(Ok(ModelStreamEvent::TextDelta("late".to_owned())))
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct HeartbeatOnlyProvider;

    impl ModelProvider for HeartbeatOnlyProvider {
        fn name(&self) -> &'static str {
            "heartbeat"
        }

        fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
            unreachable!("liveness wrapper calls invoke_observed")
        }

        fn invoke_observed(
            &self,
            _request: ModelRequest,
            observer: ProviderTransportObserver,
        ) -> Result<ProviderStream, ProviderError> {
            Ok(Box::new(HeartbeatOnlyStream { observer }))
        }
    }

    struct HeartbeatOnlyStream {
        observer: ProviderTransportObserver,
    }

    impl Iterator for HeartbeatOnlyStream {
        type Item = Result<ModelStreamEvent, ProviderError>;

        fn next(&mut self) -> Option<Self::Item> {
            while !self.observer.should_stop() {
                std::thread::sleep(Duration::from_millis(5));
                self.observer.bytes_received();
            }
            None
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct EmptyDeltaProvider;

    impl ModelProvider for EmptyDeltaProvider {
        fn name(&self) -> &'static str {
            "empty-delta"
        }

        fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
            unreachable!("liveness wrapper calls invoke_observed")
        }

        fn invoke_observed(
            &self,
            _request: ModelRequest,
            observer: ProviderTransportObserver,
        ) -> Result<ProviderStream, ProviderError> {
            Ok(Box::new(EmptyDeltaStream { observer }))
        }
    }

    struct EmptyDeltaStream {
        observer: ProviderTransportObserver,
    }

    impl Iterator for EmptyDeltaStream {
        type Item = Result<ModelStreamEvent, ProviderError>;

        fn next(&mut self) -> Option<Self::Item> {
            if self.observer.should_stop() {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
            Some(Ok(ModelStreamEvent::TextDelta(String::new())))
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct OpaqueArtifactProvider;

    impl ModelProvider for OpaqueArtifactProvider {
        fn name(&self) -> &'static str {
            "opaque-artifact"
        }

        fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
            unreachable!("liveness wrapper calls invoke_observed")
        }

        fn invoke_observed(
            &self,
            _request: ModelRequest,
            observer: ProviderTransportObserver,
        ) -> Result<ProviderStream, ProviderError> {
            Ok(Box::new(OpaqueArtifactStream { observer }))
        }
    }

    struct OpaqueArtifactStream {
        observer: ProviderTransportObserver,
    }

    impl Iterator for OpaqueArtifactStream {
        type Item = Result<ModelStreamEvent, ProviderError>;

        fn next(&mut self) -> Option<Self::Item> {
            if self.observer.should_stop() {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
            Some(Ok(ModelStreamEvent::ReasoningDelta(
                ReasoningChunk::opaque_artifact("provider-owned"),
            )))
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct EarlyBytesProvider;

    impl ModelProvider for EarlyBytesProvider {
        fn name(&self) -> &'static str {
            "early-bytes"
        }

        fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
            unreachable!("liveness wrapper calls invoke_observed")
        }

        fn invoke_observed(
            &self,
            _request: ModelRequest,
            observer: ProviderTransportObserver,
        ) -> Result<ProviderStream, ProviderError> {
            // Deterministically record body activity before the worker can
            // enqueue ResponseOpened. The consumer must still publish the
            // public lifecycle in protocol order.
            observer.bytes_received();
            Ok(Box::new(std::iter::once(Ok(ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            }))))
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct EarlyBytesThenErrorProvider;

    impl ModelProvider for EarlyBytesThenErrorProvider {
        fn name(&self) -> &'static str {
            "early-bytes-error"
        }

        fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
            unreachable!("liveness wrapper calls invoke_observed")
        }

        fn invoke_observed(
            &self,
            _request: ModelRequest,
            observer: ProviderTransportObserver,
        ) -> Result<ProviderStream, ProviderError> {
            // This models adapter-local bytes observed before the adapter can
            // establish a response. Attempt teardown must not promote those
            // private timings into a FirstByte lifecycle event.
            observer.bytes_received();
            Err(ProviderError::transport("open failed after early bytes"))
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct OpenErrorProvider;

    impl ModelProvider for OpenErrorProvider {
        fn name(&self) -> &'static str {
            "open-error"
        }

        fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
            Err(ProviderError::rejected("open failed"))
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct PanicProvider;

    impl ModelProvider for PanicProvider {
        fn name(&self) -> &'static str {
            "panic"
        }

        fn invoke(&self, _request: ModelRequest) -> Result<ProviderStream, ProviderError> {
            panic!("provider fixture panic")
        }
    }

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

    #[test]
    fn interruptible_stream_keeps_an_open_error_after_the_worker_exits() {
        let providers = ProviderSet::single(OpenErrorProvider);
        let mut stream = providers
            .invoke_interruptibly(
                "open-error",
                request(),
                CancellationCheck::new(|| false),
                liveness(100, 100, 100),
                ProviderAttemptObserver::default(),
            )
            .expect("worker starts");

        let error = stream
            .next()
            .expect("buffered provider error")
            .expect_err("provider error");

        assert_eq!(error.category(), ProviderErrorCategory::Rejected);
        assert!(error.attempt_id().is_some());
        assert!(stream.next().is_none());
    }

    #[test]
    fn provider_panic_retains_unknown_outcome_error_after_worker_exit() {
        let providers = ProviderSet::single(PanicProvider);
        let mut stream = providers
            .invoke_interruptibly(
                "panic",
                request(),
                CancellationCheck::new(|| false),
                liveness(100, 100, 100),
                ProviderAttemptObserver::default(),
            )
            .expect("worker starts");

        let error = stream
            .next()
            .expect("panic error")
            .expect_err("provider panic");

        assert!(error.request_outcome_unknown());
        assert!(error.to_string().contains("worker panicked"));
        assert!(error.attempt_id().is_some());
        assert!(stream.next().is_none());
    }

    #[test]
    fn provider_open_is_bounded_by_the_response_header_deadline() {
        let providers = ProviderSet::single(DelayedInvokeProvider {
            delay: Duration::from_millis(200),
        });
        let (observer, events) = attempt_events();
        let started = Instant::now();
        let mut stream = providers
            .invoke_interruptibly(
                "delayed",
                request(),
                CancellationCheck::new(|| false),
                liveness(25, 100, 100),
                observer,
            )
            .expect("worker starts");

        let error = stream.next().expect("timeout").expect_err("header timeout");

        assert_eq!(
            error.timeout_stage(),
            Some(ProviderTimeoutStage::ResponseHeaders)
        );
        assert!(started.elapsed() < Duration::from_millis(150));
        let events = events.lock().expect("attempt events");
        assert!(matches!(
            events.first(),
            Some(ProviderAttemptEvent::Started { .. })
        ));
        assert!(matches!(
            events.last(),
            Some(ProviderAttemptEvent::Ended(ProviderAttemptSummary {
                outcome: ProviderAttemptOutcome::TimedOut(ProviderTimeoutStage::ResponseHeaders),
                ..
            }))
        ));
    }

    #[test]
    fn opened_response_is_bounded_by_the_first_byte_deadline() {
        let providers = ProviderSet::single(DelayedFirstEventProvider {
            delay: Duration::from_millis(200),
        });
        let mut stream = providers
            .invoke_interruptibly(
                "delayed",
                request(),
                CancellationCheck::new(|| false),
                liveness(100, 25, 200),
                ProviderAttemptObserver::default(),
            )
            .expect("worker starts");

        let error = stream
            .next()
            .expect("timeout")
            .expect_err("first-byte timeout");

        assert_eq!(error.timeout_stage(), Some(ProviderTimeoutStage::FirstByte));
    }

    #[test]
    fn first_byte_stage_owns_no_byte_timeout_regardless_of_semantic_limit() {
        let providers = ProviderSet::single(DelayedFirstEventProvider {
            delay: Duration::from_millis(200),
        });
        let mut stream = providers
            .invoke_interruptibly(
                "delayed",
                request(),
                CancellationCheck::new(|| false),
                // Semantic idle is intentionally shorter. It does not begin
                // until the first byte arrives, so it cannot mislabel this
                // no-byte stall.
                liveness(100, 35, 5),
                ProviderAttemptObserver::default(),
            )
            .expect("worker starts");

        let error = stream.next().expect("timeout").expect_err("byte timeout");

        assert_eq!(error.timeout_stage(), Some(ProviderTimeoutStage::FirstByte));
    }

    #[test]
    fn early_transport_bytes_preserve_attempt_stage_order() {
        let providers = ProviderSet::single(EarlyBytesProvider);
        let (observer, events) = attempt_events();
        let stream = providers
            .invoke_interruptibly(
                "early-bytes",
                request(),
                CancellationCheck::new(|| false),
                liveness(100, 100, 100),
                observer,
            )
            .expect("worker starts");

        let output = stream.collect::<Result<Vec<_>, _>>().expect("response");

        assert!(matches!(
            output.as_slice(),
            [ModelStreamEvent::Finished { .. }]
        ));
        let events = events.lock().expect("attempt events");
        assert!(matches!(
            events.as_slice(),
            [
                ProviderAttemptEvent::Started { .. },
                ProviderAttemptEvent::ResponseHeaders { .. },
                ProviderAttemptEvent::FirstByte { .. },
                ProviderAttemptEvent::FirstSemantic { .. },
                ProviderAttemptEvent::Ended(ProviderAttemptSummary {
                    outcome: ProviderAttemptOutcome::Completed,
                    ..
                }),
            ]
        ));
        let ProviderAttemptEvent::Ended(summary) = events.last().expect("attempt summary") else {
            unreachable!("event shape asserted above");
        };
        let response_headers_ms = summary
            .response_headers_ms
            .expect("response headers timing");
        let first_byte_ms = summary.first_byte_ms.expect("first byte timing");
        let first_semantic_ms = summary.first_semantic_ms.expect("first semantic timing");
        assert!(response_headers_ms <= first_byte_ms);
        assert!(first_byte_ms <= first_semantic_ms);
    }

    #[test]
    fn early_transport_bytes_are_not_published_when_open_fails() {
        let providers = ProviderSet::single(EarlyBytesThenErrorProvider);
        let (observer, events) = attempt_events();
        let mut stream = providers
            .invoke_interruptibly(
                "early-bytes-error",
                request(),
                CancellationCheck::new(|| false),
                liveness(100, 100, 100),
                observer,
            )
            .expect("worker starts");

        let error = stream.next().expect("provider error").expect_err("failure");

        assert_eq!(error.category(), ProviderErrorCategory::Transport);
        assert!(stream.next().is_none());
        let events = events.lock().expect("attempt events");
        assert!(matches!(
            events.as_slice(),
            [
                ProviderAttemptEvent::Started { .. },
                ProviderAttemptEvent::Ended(ProviderAttemptSummary {
                    outcome: ProviderAttemptOutcome::Failed,
                    response_headers_ms: None,
                    first_byte_ms: None,
                    ..
                }),
            ]
        ));
    }

    #[test]
    fn raw_transport_heartbeats_do_not_reset_semantic_idle() {
        let providers = ProviderSet::single(HeartbeatOnlyProvider);
        let (observer, events) = attempt_events();
        let mut stream = providers
            .invoke_interruptibly(
                "heartbeat",
                request(),
                CancellationCheck::new(|| false),
                liveness(100, 100, 30),
                observer,
            )
            .expect("worker starts");

        let error = stream
            .next()
            .expect("timeout")
            .expect_err("semantic idle timeout");

        assert_eq!(
            error.timeout_stage(),
            Some(ProviderTimeoutStage::SemanticIdle)
        );
        let events = events.lock().expect("attempt events");
        assert!(events
            .iter()
            .any(|event| matches!(event, ProviderAttemptEvent::FirstByte { .. })));
        assert!(!events
            .iter()
            .any(|event| matches!(event, ProviderAttemptEvent::FirstSemantic { .. })));
    }

    #[test]
    fn empty_model_deltas_do_not_reset_semantic_idle() {
        let providers = ProviderSet::single(EmptyDeltaProvider);
        let (observer, events) = attempt_events();
        let mut stream = providers
            .invoke_interruptibly(
                "empty-delta",
                request(),
                CancellationCheck::new(|| false),
                liveness(100, 100, 30),
                observer,
            )
            .expect("worker starts");

        let error = loop {
            match stream.next().expect("event or timeout") {
                Ok(ModelStreamEvent::TextDelta(delta)) => assert!(delta.is_empty()),
                Ok(event) => panic!("unexpected event: {event:?}"),
                Err(error) => break error,
            }
        };

        assert_eq!(
            error.timeout_stage(),
            Some(ProviderTimeoutStage::SemanticIdle)
        );
        let events = events.lock().expect("attempt events");
        assert!(events
            .iter()
            .any(|event| matches!(event, ProviderAttemptEvent::FirstByte { .. })));
        assert!(!events
            .iter()
            .any(|event| matches!(event, ProviderAttemptEvent::FirstSemantic { .. })));
    }

    #[test]
    fn opaque_reasoning_artifacts_do_not_reset_semantic_idle() {
        let providers = ProviderSet::single(OpaqueArtifactProvider);
        let (observer, events) = attempt_events();
        let mut stream = providers
            .invoke_interruptibly(
                "opaque-artifact",
                request(),
                CancellationCheck::new(|| false),
                liveness(100, 100, 30),
                observer,
            )
            .expect("worker starts");

        let error = loop {
            match stream.next().expect("event or timeout") {
                Ok(ModelStreamEvent::ReasoningDelta(chunk)) => {
                    assert!(chunk.content.is_empty());
                    assert!(chunk.artifact.is_some());
                }
                Ok(event) => panic!("unexpected event: {event:?}"),
                Err(error) => break error,
            }
        };

        assert_eq!(
            error.timeout_stage(),
            Some(ProviderTimeoutStage::SemanticIdle)
        );
        let events = events.lock().expect("attempt events");
        assert!(events
            .iter()
            .any(|event| matches!(event, ProviderAttemptEvent::FirstByte { .. })));
        assert!(!events
            .iter()
            .any(|event| matches!(event, ProviderAttemptEvent::FirstSemantic { .. })));
    }

    #[test]
    fn productive_stream_has_no_total_duration_cap() {
        let providers =
            ProviderSet::single(ScriptedProvider::new(vec![FixtureResponse::Stream(vec![
                ScriptedStreamStep::SleepMs(20),
                ScriptedStreamStep::Event(ModelStreamEvent::TextDelta("one".to_owned())),
                ScriptedStreamStep::SleepMs(20),
                ScriptedStreamStep::Event(ModelStreamEvent::TextDelta("two".to_owned())),
                ScriptedStreamStep::SleepMs(20),
                ScriptedStreamStep::Event(ModelStreamEvent::TextDelta("three".to_owned())),
                ScriptedStreamStep::SleepMs(20),
                ScriptedStreamStep::Event(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None,
                }),
            ])]));
        let started = Instant::now();
        let stream = providers
            .invoke_interruptibly(
                "fixture",
                request(),
                CancellationCheck::new(|| false),
                liveness(100, 50, 50),
                ProviderAttemptObserver::default(),
            )
            .expect("worker starts");

        let events = stream
            .collect::<Result<Vec<_>, _>>()
            .expect("productive stream");

        assert!(started.elapsed() > Duration::from_millis(50));
        assert_eq!(events.len(), 4);
        assert!(matches!(
            events.last(),
            Some(ModelStreamEvent::Finished { .. })
        ));
    }

    #[test]
    fn cancellation_detaches_from_a_blocked_provider_promptly() {
        let providers = ProviderSet::single(DelayedInvokeProvider {
            delay: Duration::from_secs(1),
        });
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation_probe = Arc::clone(&cancelled);
        let mut stream = providers
            .invoke_interruptibly(
                "delayed",
                request(),
                CancellationCheck::new(move || cancellation_probe.load(Ordering::Acquire)),
                liveness(5_000, 5_000, 5_000),
                ProviderAttemptObserver::default(),
            )
            .expect("worker starts");
        let cancel_flag = Arc::clone(&cancelled);
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            cancel_flag.store(true, Ordering::Release);
        });
        let started = Instant::now();

        assert!(stream.next().is_none());

        cancel.join().expect("cancel thread");
        assert!(started.elapsed() < Duration::from_millis(150));
    }
}
