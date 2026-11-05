//! Gateway model discovery for custom providers with `"discovery": true`
//! (ADR 0025 section 2). `/v1/models` is the generic OpenAI-compatible
//! listing. Reading LiteLLM's `/model/info` is the boundary contract's
//! documented exception (ADR 0025 section 5): its exit is an extension once
//! extensions can contribute providers.

use crate::auth::SecretString;
use crate::provider_config::{model_id_is_valid, ApiFamily, CustomModelConfig};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::time::{Duration, Instant};
use url::Url;

pub const MODEL_INFO_PATH: &str = "/model/info";
pub const MODELS_PATH: &str = "/v1/models";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const RUN_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;

/// One successful discovery run: the endpoint the models came from, the
/// mapped models sorted by id, and the routes mapping dropped or demoted.
#[derive(Clone, Debug, PartialEq)]
pub struct GatewayDiscovery {
    pub endpoint: &'static str,
    pub models: Vec<CustomModelConfig>,
    pub notes: Vec<String>,
}

/// Runs discovery against the gateway root of `base_url` (its `/v1` removed)
/// with the provider's resolved headers. Requests stay at the base URL's
/// origin, follow no redirects, and share one overall deadline. Errors name
/// the endpoint and the status or transport error, never a response body.
pub(crate) fn discover(
    base_url: &str,
    api_family: ApiFamily,
    headers: &BTreeMap<String, SecretString>,
) -> Result<GatewayDiscovery, String> {
    let root = gateway_root(base_url)?;
    let fetcher = Fetcher {
        agent: ureq::AgentBuilder::new().redirects(0).build(),
        root,
        headers,
        deadline: Instant::now() + RUN_TIMEOUT,
    };
    let model_info = fetcher.get(MODEL_INFO_PATH);
    if let Some(rows) = model_info.as_ref().ok().and_then(model_info_rows) {
        let has_wildcards = rows.iter().any(|row| row_name(row).contains('*'));
        let listed_ids = has_wildcards.then(|| {
            fetcher
                .get(MODELS_PATH)
                .and_then(|body| model_list_ids(&body, &fetcher.url(MODELS_PATH)))
        });
        return map_model_info(&rows, listed_ids, &fetcher.url(MODEL_INFO_PATH));
    }
    let models = fetcher
        .get(MODELS_PATH)
        .map_err(|error| match &model_info {
            Ok(_) => format!(
                "{error} (GET {} did not return model rows)",
                fetcher.url(MODEL_INFO_PATH)
            ),
            Err(info_error) => format!("{error} ({info_error})"),
        })?;
    map_model_list(&models, api_family, &fetcher.url(MODELS_PATH))
}

/// `base_url` with trailing slashes and the trailing `/v1` removed.
fn gateway_root(base_url: &str) -> Result<Url, String> {
    let mut url =
        Url::parse(base_url).map_err(|_| "discovery base_url is not a valid URL".to_owned())?;
    let path = url.path().trim_end_matches('/');
    let root = path.strip_suffix("/v1").unwrap_or(path).to_owned();
    url.set_path(&root);
    Ok(url)
}

struct Fetcher<'a> {
    agent: ureq::Agent,
    root: Url,
    headers: &'a BTreeMap<String, SecretString>,
    deadline: Instant,
}

impl Fetcher<'_> {
    fn url(&self, path: &str) -> String {
        let mut url = self.root.clone();
        url.set_path(&format!("{}{path}", self.root.path().trim_end_matches('/')));
        url.to_string()
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        let url = self.url(path);
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| format!("GET {url} failed: timed out"))?;
        let mut request = self
            .agent
            .get(&url)
            .timeout(remaining.min(REQUEST_TIMEOUT))
            .set("Accept", "application/json");
        for (name, value) in self.headers {
            request = request.set(name, value.expose());
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(status, _)) => {
                return Err(format!("GET {url} returned HTTP {status}"))
            }
            Err(ureq::Error::Transport(transport)) => {
                return Err(format!(
                    "GET {url} failed: {}",
                    transport_reason(&transport)
                ))
            }
        };
        // A redirect is not followed, so it arrives here as a non-200 answer.
        if response.status() != 200 {
            return Err(format!("GET {url} returned HTTP {}", response.status()));
        }
        let declared = response
            .header("Content-Length")
            .and_then(|value| value.parse::<u64>().ok());
        if declared.is_some_and(|length| length > MAX_BODY_BYTES) {
            return Err(format!("GET {url} returned a body over 4 MiB"));
        }
        let mut body = Vec::new();
        response
            .into_reader()
            .take(MAX_BODY_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(|error| format!("GET {url} failed: {}", io_reason(&error)))?;
        if body.len() as u64 > MAX_BODY_BYTES {
            return Err(format!("GET {url} returned a body over 4 MiB"));
        }
        serde_json::from_slice(&body)
            .map_err(|_| format!("GET {url} returned a body that is not JSON"))
    }
}

fn transport_reason(transport: &ureq::Transport) -> String {
    let timed_out = std::error::Error::source(transport)
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .is_some_and(io_timed_out);
    if timed_out {
        return "timed out".to_owned();
    }
    match transport.kind() {
        ureq::ErrorKind::Dns => "DNS lookup failed".to_owned(),
        ureq::ErrorKind::ConnectionFailed => "connection failed (connect or TLS)".to_owned(),
        kind => kind.to_string().to_ascii_lowercase(),
    }
}

fn io_reason(error: &std::io::Error) -> &'static str {
    if io_timed_out(error) {
        "timed out"
    } else {
        "reading the response failed"
    }
}

fn io_timed_out(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

/// The `/model/info` rows: list entries that are objects with a string
/// `model_name`, from a `{"data": [...]}` body or a bare list. `None` when
/// there are none, which sends discovery to `/v1/models`.
fn model_info_rows(body: &Value) -> Option<Vec<Value>> {
    let list = body.get("data").unwrap_or(body).as_array()?;
    let rows = list
        .iter()
        .filter(|row| row.get("model_name").is_some_and(Value::is_string))
        .cloned()
        .collect::<Vec<_>>();
    (!rows.is_empty()).then_some(rows)
}

fn row_name(row: &Value) -> &str {
    row.get("model_name").and_then(Value::as_str).unwrap_or("")
}

/// The ids of a `/v1/models` body, `{"data": [{"id": ...}]}`.
fn model_list_ids(body: &Value, url: &str) -> Result<Vec<String>, String> {
    let ids = body
        .get("data")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("id").and_then(Value::as_str))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .ok_or_else(|| format!("GET {url} returned a body that is not a model list"))?;
    if ids.is_empty() {
        return Err(format!("GET {url} returned an empty model list"));
    }
    Ok(ids)
}

/// One `/model/info` deployment of a route.
struct Deployment<'a> {
    info: Option<&'a Map<String, Value>>,
    /// `litellm_params.model`, else `model_info.key`; for a route expanded
    /// from a wildcard, the expanded id.
    underlying_model: String,
}

impl<'a> Deployment<'a> {
    fn from_row(row: &'a Value) -> Self {
        let info = row.get("model_info").and_then(Value::as_object);
        let underlying_model = row
            .pointer("/litellm_params/model")
            .and_then(Value::as_str)
            .or_else(|| {
                info.and_then(|info| info.get("key"))
                    .and_then(Value::as_str)
            })
            .unwrap_or_default()
            .to_owned();
        Self {
            info,
            underlying_model,
        }
    }

    fn info_str(&self, field: &str) -> Option<&str> {
        self.info?.get(field)?.as_str()
    }

    fn mode(&self) -> Option<&str> {
        self.info_str("mode")
    }

    /// Chat routes: mode `chat`, `responses`, or unset. Embedding, image,
    /// completion, audio, rerank, and every other mode are dropped.
    fn is_chat(&self) -> bool {
        matches!(self.mode(), None | Some("chat" | "responses"))
    }

    fn api_family(&self) -> ApiFamily {
        match self.mode() {
            Some("chat") if self.claude_behind_anthropic_host() => ApiFamily::AnthropicMessages,
            Some("responses") => ApiFamily::OpenAiResponses,
            _ => ApiFamily::OpenAiChatCompletions,
        }
    }

    fn claude_behind_anthropic_host(&self) -> bool {
        match self.info_str("litellm_provider") {
            Some("anthropic" | "vertex_ai-anthropic_models") => true,
            Some("bedrock" | "bedrock_converse" | "vertex_ai") => self
                .underlying_model
                .to_ascii_lowercase()
                .contains("claude"),
            _ => false,
        }
    }

    fn positive(&self, field: &str) -> Option<u64> {
        self.info?.get(field)?.as_u64().filter(|value| *value > 0)
    }

    fn flag(&self, field: &str) -> Option<bool> {
        self.info?.get(field)?.as_bool()
    }
}

fn map_model_info(
    rows: &[Value],
    listed_ids: Option<Result<Vec<String>, String>>,
    url: &str,
) -> Result<GatewayDiscovery, String> {
    let mut routes = BTreeMap::<String, Vec<Deployment<'_>>>::new();
    // Every explicit name, chat or not, so a wildcard never re-adds an id
    // whose own row was dropped as non-chat.
    let mut explicit_names = BTreeSet::<&str>::new();
    // Wildcards keep their non-chat rows so the longest-prefix match sees
    // them; expansion drops those rows afterwards.
    let mut wildcards = BTreeMap::<&str, Vec<&Value>>::new();
    for row in rows {
        let name = row_name(row);
        if name.contains('*') {
            wildcards.entry(name).or_default().push(row);
            continue;
        }
        explicit_names.insert(name);
        let deployment = Deployment::from_row(row);
        if deployment.is_chat() {
            routes.entry(name.to_owned()).or_default().push(deployment);
        }
    }
    let mut notes = Vec::new();
    match listed_ids {
        Some(Ok(ids)) => expand_wildcards(&mut routes, &explicit_names, &wildcards, ids),
        Some(Err(error)) => {
            let dropped = wildcards
                .iter()
                .filter(|(_, rows)| rows.iter().any(|row| Deployment::from_row(row).is_chat()))
                .map(|(name, _)| *name)
                .collect::<Vec<_>>();
            if !dropped.is_empty() {
                notes.push(format!(
                    "dropped wildcard routes {} because {error}",
                    dropped.join(", ")
                ));
            }
        }
        None => {}
    }
    let mut invalid = Vec::new();
    let mut disagreeing = Vec::new();
    let mut models = Vec::new();
    for (id, deployments) in &routes {
        if !model_id_is_valid(id) {
            invalid.push(id.as_str());
            continue;
        }
        let family = deployments[0].api_family();
        let api_family = if deployments
            .iter()
            .all(|deployment| deployment.api_family() == family)
        {
            family
        } else {
            disagreeing.push(id.as_str());
            ApiFamily::OpenAiChatCompletions
        };
        models.push(route_model(id, api_family, deployments));
    }
    if !invalid.is_empty() {
        notes.push(format!(
            "dropped routes {} because a model id must not be empty, padded, or contain `::`",
            invalid.join(", ")
        ));
    }
    if !disagreeing.is_empty() {
        notes.push(format!(
            "routed {} to openai_chat_completions because their deployments disagree on the API family",
            disagreeing.join(", ")
        ));
    }
    if models.is_empty() {
        return Err(format!("GET {url} lists no chat models"));
    }
    Ok(GatewayDiscovery {
        endpoint: MODEL_INFO_PATH,
        models,
        notes,
    })
}

/// Adds each listed id that no explicit row names and a wildcard route
/// matches, with that route's chat deployments and the id as underlying
/// model. An id whose best-matching route has no chat deployment is dropped.
fn expand_wildcards<'a>(
    routes: &mut BTreeMap<String, Vec<Deployment<'a>>>,
    explicit_names: &BTreeSet<&str>,
    wildcards: &BTreeMap<&'a str, Vec<&'a Value>>,
    ids: Vec<String>,
) {
    for id in ids {
        if explicit_names.contains(id.as_str()) || routes.contains_key(&id) {
            continue;
        }
        if let Some(rows) = longest_prefix_match(wildcards, &id) {
            let deployments = rows
                .iter()
                .map(|row| Deployment {
                    underlying_model: id.clone(),
                    ..Deployment::from_row(row)
                })
                .filter(Deployment::is_chat)
                .collect::<Vec<_>>();
            if !deployments.is_empty() {
                routes.insert(id, deployments);
            }
        }
    }
}

/// The wildcard route matching `id` with the longest literal prefix (the
/// text before its first `*`).
fn longest_prefix_match<'a, 'b>(
    wildcards: &'b BTreeMap<&'a str, Vec<&'a Value>>,
    id: &str,
) -> Option<&'b Vec<&'a Value>> {
    wildcards
        .iter()
        .filter(|(pattern, _)| wildcard_matches(pattern, id))
        .max_by_key(|(pattern, _)| pattern.find('*').unwrap_or(pattern.len()))
        .map(|(_, rows)| rows)
}

/// `*` matches any run of characters, including none.
fn wildcard_matches(pattern: &str, id: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = id.strip_prefix(first) else {
        return false;
    };
    let mut parts = parts.collect::<Vec<_>>();
    let last = parts.pop().unwrap_or_default();
    for part in parts {
        match rest.find(part) {
            Some(index) => rest = &rest[index + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

/// Merges a route's deployments conservatively: limits are the minimum and
/// support flags are true only when every deployment says so; a field
/// missing from any deployment is omitted.
fn route_model(
    id: &str,
    api_family: ApiFamily,
    deployments: &[Deployment<'_>],
) -> CustomModelConfig {
    let minimum = |field: &str| {
        deployments
            .iter()
            .map(|deployment| deployment.positive(field))
            .collect::<Option<Vec<_>>>()
            .and_then(|values| values.into_iter().min())
    };
    let all = |field: &str| {
        deployments
            .iter()
            .map(|deployment| deployment.flag(field))
            .collect::<Option<Vec<_>>>()
            .map(|values| values.into_iter().all(|value| value))
    };
    let supports_reasoning = all("supports_reasoning");
    let compat = (api_family == ApiFamily::OpenAiChatCompletions
        && supports_reasoning == Some(true))
    .then(|| json!({"reasoning": {"request_format": "openai_reasoning_effort"}}));
    CustomModelConfig {
        id: id.to_owned(),
        display_name: id.to_owned(),
        api_family,
        context_window_tokens: minimum("max_input_tokens"),
        max_output_tokens: minimum("max_output_tokens"),
        supports_tools: all("supports_function_calling"),
        supports_reasoning,
        compat,
        discovered: true,
    }
}

/// Maps a `/v1/models` listing: every id, with the provider's family and no
/// declared metadata, like an unlisted `--model`.
fn map_model_list(
    body: &Value,
    api_family: ApiFamily,
    url: &str,
) -> Result<GatewayDiscovery, String> {
    let ids = model_list_ids(body, url)?;
    let (valid, invalid): (Vec<_>, Vec<_>) = ids.into_iter().partition(|id| model_id_is_valid(id));
    let mut notes = Vec::new();
    if !invalid.is_empty() {
        notes.push(format!(
            "dropped routes {} because a model id must not be empty, padded, or contain `::`",
            invalid.join(", ")
        ));
    }
    let mut models = valid
        .into_iter()
        .map(|id| CustomModelConfig {
            display_name: id.clone(),
            id,
            api_family,
            context_window_tokens: None,
            max_output_tokens: None,
            supports_tools: None,
            supports_reasoning: None,
            compat: None,
            discovered: true,
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| left.id.cmp(&right.id));
    models.dedup_by(|left, right| left.id == right.id);
    if models.is_empty() {
        return Err(format!("GET {url} returned an empty model list"));
    }
    Ok(GatewayDiscovery {
        endpoint: MODELS_PATH,
        models,
        notes,
    })
}

#[cfg(test)]
pub(crate) fn map_model_info_for_test(
    model_info: &Value,
    models: Option<Result<Value, String>>,
) -> Result<GatewayDiscovery, String> {
    let rows = model_info_rows(model_info).ok_or("not model rows")?;
    let listed = models.map(|body| body.and_then(|body| model_list_ids(&body, MODELS_PATH)));
    map_model_info(&rows, listed, MODEL_INFO_PATH)
}

#[cfg(test)]
pub(crate) fn map_model_list_for_test(
    body: &Value,
    api_family: ApiFamily,
) -> Result<GatewayDiscovery, String> {
    map_model_list(body, api_family, MODELS_PATH)
}
