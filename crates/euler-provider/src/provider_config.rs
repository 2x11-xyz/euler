//! Custom provider config parsing for `~/.euler/providers.json`.

use crate::catalog::canonical_provider_id;
use serde::de::{Deserializer, Error, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::Ipv6Addr;
use url::{Host, Url};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProviderConfigRegistry {
    pub providers: BTreeMap<String, CustomProviderConfig>,
}

impl ProviderConfigRegistry {
    pub fn with_json(contents: &str) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let contents = contents.trim_start_matches('\u{feff}');
        if contents.trim().is_empty() {
            warnings.push("providers.json is empty; no custom providers loaded".to_owned());
            return (Self::default(), warnings);
        }
        let root: RawRoot = match serde_json::from_str(contents) {
            Ok(root) => root,
            Err(error) => {
                warnings.push(format!(
                    "providers.json is not valid JSON ({error}); no custom providers loaded"
                ));
                return (Self::default(), warnings);
            }
        };
        if !valid_version(root.version.as_ref(), &mut warnings) {
            return (Self::default(), warnings);
        }
        warn_unknown(
            root.extra.keys(),
            &["version", "providers"],
            "root",
            &mut warnings,
        );
        let mut registry = Self::default();
        for (id, value) in root
            .providers
            .map_or_else(BTreeMap::new, |providers| providers.0)
        {
            if let Some(provider) = parse_provider(&id, &value, &mut warnings) {
                registry.providers.insert(id, provider);
            }
        }
        (registry, warnings)
    }

    pub fn providers(&self) -> impl Iterator<Item = &CustomProviderConfig> {
        self.providers.values()
    }

    pub fn provider(&self, id: &str) -> Option<&CustomProviderConfig> {
        self.providers.get(id)
    }
}

/// The environment variables `providers.json` references as `$NAME` or
/// `${NAME}` in a provider's `api_key` or header values: Euler's own
/// credentials, which no command Euler runs inherits (ADR 0026 sections 1
/// and 2). Every provider entry counts, including one
/// [`ProviderConfigRegistry::with_json`] ignores as malformed and each entry
/// of a repeated provider id or `providers` key, so a broken entry's secret
/// is withheld too. A file that is not valid JSON names none.
pub fn referenced_env_names(contents: &str) -> BTreeSet<String> {
    let Ok(ProviderEntries(entries)) =
        serde_json::from_str::<ProviderEntries>(contents.trim_start_matches('\u{feff}'))
    else {
        return BTreeSet::new();
    };
    entries
        .iter()
        .filter_map(Value::as_object)
        .flat_map(|provider| {
            let headers = provider
                .get("headers")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(Map::values);
            provider.get("api_key").into_iter().chain(headers)
        })
        .filter_map(Value::as_str)
        .filter_map(crate::custom_provider::secret_spec_env_name)
        .map(str::to_owned)
        .collect()
}

/// Every provider entry of a `providers.json` root object, in file order.
/// Unlike a map, it keeps each entry of a repeated provider id and of a
/// repeated `providers` key; a `providers` value that is not an object adds
/// no entries.
struct ProviderEntries(Vec<Value>);

impl<'de> Deserialize<'de> for ProviderEntries {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RootVisitor;
        impl<'de> Visitor<'de> for RootVisitor {
            type Value = ProviderEntries;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a providers.json object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some(key) = access.next_key::<String>()? {
                    if key == "providers" {
                        entries.extend(access.next_value::<ProvidersValue>()?.0);
                    } else {
                        access.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(ProviderEntries(entries))
            }
        }
        deserializer.deserialize_map(RootVisitor)
    }
}

/// The entries of one `providers` value: every value of an object, repeated
/// keys included, and none for any other JSON value, which must not fail the
/// parse and discard the entries of another `providers` key.
struct ProvidersValue(Vec<Value>);

impl<'de> Deserialize<'de> for ProvidersValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ProvidersVisitor;
        impl<'de> Visitor<'de> for ProvidersVisitor {
            type Value = ProvidersValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("any JSON value")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some((_, value)) = access.next_entry::<IgnoredAny, Value>()? {
                    entries.push(value);
                }
                Ok(ProvidersValue(entries))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                while access.next_element::<IgnoredAny>()?.is_some() {}
                Ok(ProvidersValue(Vec::new()))
            }

            fn visit_unit<E: Error>(self) -> Result<Self::Value, E> {
                Ok(ProvidersValue(Vec::new()))
            }

            fn visit_bool<E: Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(ProvidersValue(Vec::new()))
            }

            fn visit_i64<E: Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(ProvidersValue(Vec::new()))
            }

            fn visit_u64<E: Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(ProvidersValue(Vec::new()))
            }

            fn visit_f64<E: Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(ProvidersValue(Vec::new()))
            }

            fn visit_str<E: Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(ProvidersValue(Vec::new()))
            }
        }
        deserializer.deserialize_any(ProvidersVisitor)
    }
}

#[derive(Clone, PartialEq)]
pub struct CustomProviderConfig {
    pub id: String,
    pub api_family: ApiFamily,
    pub base_url: String,
    pub api_key: Option<String>,
    pub auth_header: bool,
    pub headers: BTreeMap<String, String>,
    pub default_model: Option<String>,
    pub default_model_error: Option<String>,
    pub models: BTreeMap<String, CustomModelConfig>,
}

impl fmt::Debug for CustomProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers = self
            .headers
            .keys()
            .map(|name| (name, "[redacted]"))
            .collect::<BTreeMap<_, _>>();
        formatter
            .debug_struct("CustomProviderConfig")
            .field("id", &self.id)
            .field("api_family", &self.api_family)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("auth_header", &self.auth_header)
            .field("headers", &headers)
            .field("default_model", &self.default_model)
            .field("default_model_error", &self.default_model_error)
            .field("models", &self.models)
            .finish()
    }
}

impl CustomProviderConfig {
    /// The wire family for `model`: its entry's family when listed, otherwise
    /// the provider's (an unlisted `--model`).
    pub fn model_api_family(&self, model: &str) -> ApiFamily {
        self.models
            .get(model)
            .map_or(self.api_family, |model| model.api_family)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApiFamily {
    OpenAiChatCompletions,
    AnthropicMessages,
    OpenAiResponses,
}

impl ApiFamily {
    const ALL: [Self; 3] = [
        Self::OpenAiChatCompletions,
        Self::AnthropicMessages,
        Self::OpenAiResponses,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiChatCompletions => "openai_chat_completions",
            Self::AnthropicMessages => "anthropic_messages",
            Self::OpenAiResponses => "openai_responses",
        }
    }

    /// The endpoint path a `base_url` may already end with for this family.
    pub(crate) fn endpoint_suffix(self) -> &'static str {
        match self {
            Self::OpenAiChatCompletions => "/chat/completions",
            Self::AnthropicMessages => "/messages",
            Self::OpenAiResponses => "/responses",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CustomModelConfig {
    pub id: String,
    pub display_name: String,
    pub api_family: ApiFamily,
    pub context_window_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub supports_tools: Option<bool>,
    pub supports_reasoning: Option<bool>,
    pub compat: Option<Value>,
}

#[derive(Deserialize)]
struct RawRoot {
    version: Option<Value>,
    providers: Option<UniqueMap<Value>>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

struct UniqueMap<V>(BTreeMap<String, V>);

impl<'de, V: Deserialize<'de>> Deserialize<'de> for UniqueMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueMapVisitor<V>(std::marker::PhantomData<V>);
        impl<'de, V: Deserialize<'de>> Visitor<'de> for UniqueMapVisitor<V> {
            type Value = UniqueMap<V>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object without duplicate keys")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut map = BTreeMap::new();
                while let Some((key, value)) = access.next_entry::<String, V>()? {
                    if map.insert(key.clone(), value).is_some() {
                        return Err(A::Error::custom(format!(
                            "duplicate provider key `{key}` in providers.json"
                        )));
                    }
                }
                Ok(UniqueMap(map))
            }
        }
        deserializer.deserialize_map(UniqueMapVisitor(std::marker::PhantomData))
    }
}

fn parse_provider(
    id: &str,
    value: &Value,
    warnings: &mut Vec<String>,
) -> Option<CustomProviderConfig> {
    let id = valid_provider_id(id, warnings)?;
    let scope = format!("provider `{id}`");
    let object = object_value(value, &scope, warnings)?;
    warn_unknown(
        object.keys(),
        &[
            "api_family",
            "base_url",
            "api_key",
            "auth_header",
            "headers",
            "default_model",
            "models",
        ],
        &scope,
        warnings,
    );
    if !object.contains_key("api_family") {
        warnings.push(format!("ignored {scope} because api_family is missing"));
        return None;
    }
    let api_family = api_family_field(object, &scope, warnings)?;
    let base_url = required_base_url(object, &scope, warnings)?;
    // The provider's family also serves `default_model_only` and any unlisted
    // `--model`, so its base_url must serve that family.
    if !base_url_serves(&base_url, api_family) {
        warnings.push(format!(
            "ignored {scope} because base_url is a full endpoint for another api_family; set api_family to the family it serves, or use the API root as base_url"
        ));
        return None;
    }
    let route = ProviderRoute {
        api_family,
        base_url: &base_url,
    };
    let auth_header = bool_field(object, "auth_header", &scope, warnings).unwrap_or(false);
    let headers = parse_headers(object.get("headers"), &scope, auth_header, warnings)?;
    let models = match object.get("models") {
        Some(models) => parse_models(models, &scope, &route, warnings)?,
        None => default_model_only(object.get("default_model"), &scope, api_family, warnings)?,
    };
    let (default_model, default_model_error) =
        parse_default_model(object.get("default_model"), &id, &models, warnings);
    Some(CustomProviderConfig {
        id,
        api_family,
        base_url,
        api_key: string_field(object, "api_key", &scope, warnings).map(str::to_owned),
        auth_header,
        headers,
        default_model,
        default_model_error,
        models,
    })
}

fn valid_provider_id(id: &str, warnings: &mut Vec<String>) -> Option<String> {
    let valid_chars = id
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if id.is_empty() || id.trim() != id || id != id.to_ascii_lowercase() || !valid_chars {
        warnings.push(format!(
            "ignored provider `{id}` because custom provider ids must be lowercase ASCII letters, numbers, '-' or '_'"
        ));
        return None;
    }
    if canonical_provider_id(id).is_some() {
        warnings.push(format!(
            "ignored provider `{id}` in providers.json because built-in provider ids and aliases are reserved"
        ));
        return None;
    }
    Some(id.to_owned())
}

fn required_base_url(
    object: &Map<String, Value>,
    scope: &str,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let url = string_field(object, "base_url", scope, warnings)?;
    match valid_base_url(url) {
        Ok(()) => Some(url.to_owned()),
        Err(reason) => {
            warnings.push(format!("ignored {scope} because base_url {reason}"));
            None
        }
    }
}

fn api_family_field(
    object: &Map<String, Value>,
    scope: &str,
    warnings: &mut Vec<String>,
) -> Option<ApiFamily> {
    let names = ApiFamily::ALL.map(ApiFamily::as_str);
    let value = choice_field(object, "api_family", scope, &names, warnings)?;
    ApiFamily::ALL
        .into_iter()
        .find(|family| family.as_str() == value)
}

/// A `base_url` is either an API root, which every family joins its own path
/// onto, or a full endpoint, which serves only the family it names.
fn base_url_serves(base_url: &str, api_family: ApiFamily) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };
    let path = url.path().trim_end_matches('/');
    ApiFamily::ALL
        .into_iter()
        .filter(|family| *family != api_family)
        .all(|family| !path.ends_with(family.endpoint_suffix()))
}

/// The provider-level transport a model entry inherits.
struct ProviderRoute<'a> {
    api_family: ApiFamily,
    base_url: &'a str,
}

fn valid_base_url(url: &str) -> Result<(), &'static str> {
    let parsed = Url::parse(url).map_err(|_| "must be a valid absolute http or https URL")?;
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("must not include query strings or fragments");
    }
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("must use http or https");
    }
    if parsed.host().is_none() {
        return Err("is missing a host");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("must not include embedded credentials");
    }
    if parsed.scheme() == "http" && !loopback_host(parsed.host().expect("checked host")) {
        return Err("uses non-loopback HTTP");
    }
    Ok(())
}

fn loopback_host(host: Host<&str>) -> bool {
    match host {
        Host::Domain(host) => host == "localhost",
        Host::Ipv4(ip) => ip.octets()[0] == 127,
        Host::Ipv6(ip) => ip == Ipv6Addr::LOCALHOST,
    }
}

fn parse_headers(
    value: Option<&Value>,
    scope: &str,
    auth_header: bool,
    warnings: &mut Vec<String>,
) -> Option<BTreeMap<String, String>> {
    let Some(value) = value else {
        return Some(BTreeMap::new());
    };
    let object = object_value(value, &format!("{scope} headers"), warnings)?;
    let mut headers = BTreeMap::new();
    let mut seen = BTreeMap::<String, String>::new();
    for (name, value) in object {
        let lower = name.to_ascii_lowercase();
        if !valid_header_name(name) {
            warnings.push(format!(
                "ignored {scope} because header `{name}` is not a valid HTTP header name"
            ));
            return None;
        }
        if let Some(previous) = seen.insert(lower.clone(), name.clone()) {
            warnings.push(format!(
                "ignored {scope} because headers `{previous}` and `{name}` differ only by case"
            ));
            return None;
        }
        if adapter_owned_header(&lower) {
            warnings.push(format!(
                "ignored {scope} because header `{name}` is owned by the provider adapter"
            ));
            return None;
        }
        if lower == "authorization" && auth_header {
            warnings.push(format!(
                "ignored {scope} because Authorization header conflicts with auth_header"
            ));
            return None;
        }
        headers.insert(
            name.clone(),
            string_value(value, &format!("{scope} header `{name}`"), warnings)?.to_owned(),
        );
    }
    Some(headers)
}

fn adapter_owned_header(lowercase_name: &str) -> bool {
    matches!(lowercase_name, "accept" | "content-type")
}

fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn parse_models(
    value: &Value,
    scope: &str,
    route: &ProviderRoute<'_>,
    warnings: &mut Vec<String>,
) -> Option<BTreeMap<String, CustomModelConfig>> {
    let Some(models) = value.as_array() else {
        warnings.push(format!(
            "ignored {scope} because models is not an array; {MODELS_FIX}"
        ));
        return None;
    };
    let mut parsed = BTreeMap::new();
    for (index, value) in models.iter().enumerate() {
        if let Some(model) = parse_model(value, &format!("{scope} model #{index}"), route, warnings)
        {
            let model_id = model.id.clone();
            if parsed.insert(model_id.clone(), model).is_some() {
                warnings.push(format!(
                    "{} model `{}` appeared more than once; last valid descriptor wins",
                    scope, model_id
                ));
            }
        }
    }
    if parsed.is_empty() {
        warnings.push(format!(
            "ignored {scope} because it has no valid models; {MODELS_FIX}"
        ));
        return None;
    }
    Some(parsed)
}

/// A provider that omits `models` offers only its `default_model`. The entry
/// declares no metadata, so it behaves like an unlisted `--model`.
fn default_model_only(
    value: Option<&Value>,
    scope: &str,
    api_family: ApiFamily,
    warnings: &mut Vec<String>,
) -> Option<BTreeMap<String, CustomModelConfig>> {
    let id = value
        .and_then(Value::as_str)
        .and_then(|id| valid_model_id(id, &format!("{scope} default_model"), warnings));
    let Some(id) = id else {
        warnings.push(format!(
            "ignored {scope} because it has no models list and no valid string default_model; {MODELS_FIX}"
        ));
        return None;
    };
    let model = CustomModelConfig {
        id: id.clone(),
        display_name: id.clone(),
        api_family,
        context_window_tokens: None,
        max_output_tokens: None,
        supports_tools: None,
        supports_reasoning: None,
        compat: None,
    };
    Some(BTreeMap::from([(id, model)]))
}

fn parse_model(
    value: &Value,
    scope: &str,
    route: &ProviderRoute<'_>,
    warnings: &mut Vec<String>,
) -> Option<CustomModelConfig> {
    let object = object_value(value, scope, warnings)?;
    warn_unknown(
        object.keys(),
        &[
            "id",
            "display_name",
            "api_family",
            "context_window_tokens",
            "max_output_tokens",
            "supports_tools",
            "supports_reasoning",
            "compat",
        ],
        scope,
        warnings,
    );
    let id = valid_model_id(
        string_field(object, "id", scope, warnings)?,
        scope,
        warnings,
    )?;
    let api_family = match object.get("api_family") {
        None => route.api_family,
        Some(_) => api_family_field(object, scope, warnings)?,
    };
    if !base_url_serves(route.base_url, api_family) {
        warnings.push(format!(
            "ignored {scope} because the provider base_url is a full endpoint for another api_family"
        ));
        return None;
    }
    let compat = match api_family {
        ApiFamily::OpenAiChatCompletions => parse_compat(object.get("compat"), scope, warnings),
        ApiFamily::AnthropicMessages | ApiFamily::OpenAiResponses => {
            if object.contains_key("compat") {
                warnings.push(format!(
                    "ignored {scope} compat because it applies only to openai_chat_completions"
                ));
            }
            None
        }
    };
    Some(CustomModelConfig {
        display_name: string_field(object, "display_name", scope, warnings)
            .unwrap_or(&id)
            .to_owned(),
        api_family,
        context_window_tokens: positive_u64(object, "context_window_tokens", scope, warnings),
        max_output_tokens: positive_u64(object, "max_output_tokens", scope, warnings),
        supports_tools: bool_field(object, "supports_tools", scope, warnings),
        supports_reasoning: bool_field(object, "supports_reasoning", scope, warnings),
        compat,
        id,
    })
}

fn parse_default_model(
    value: Option<&Value>,
    provider_id: &str,
    models: &BTreeMap<String, CustomModelConfig>,
    warnings: &mut Vec<String>,
) -> (Option<String>, Option<String>) {
    let Some(value) = value else {
        return (None, None);
    };
    let Some(model) = value.as_str() else {
        let warning = format!("provider `{provider_id}` default_model is not a string");
        warnings.push(warning.clone());
        return (None, Some(warning));
    };
    if models.contains_key(model) {
        (Some(model.to_owned()), None)
    } else {
        let warning =
            format!("providers.{provider_id}.default_model references unknown model `{model}`");
        warnings.push(warning.clone());
        (None, Some(warning))
    }
}

fn parse_compat(value: Option<&Value>, scope: &str, warnings: &mut Vec<String>) -> Option<Value> {
    let value = value?;
    let Some(object) = value.as_object() else {
        warnings.push(format!(
            "ignored {scope} compat because it is not an object"
        ));
        return None;
    };
    let cscope = format!("{scope} compat");
    warn_unknown(object.keys(), COMPAT_FIELDS, &cscope, warnings);
    redacted_choice_field(
        object,
        "max_tokens_field",
        &cscope,
        MAX_TOKEN_FIELDS,
        warnings,
    );
    bool_field(object, "supports_developer_role", &cscope, warnings);
    bool_field(object, "supports_stream_usage", &cscope, warnings);
    bool_field(object, "requires_tool_result_name", &cscope, warnings);
    bool_field(
        object,
        "requires_assistant_after_tool_result",
        &cscope,
        warnings,
    );
    bool_field(object, "supports_strict_tools", &cscope, warnings);
    if let Some(reasoning) = object.get("reasoning") {
        lint_reasoning(reasoning, scope, warnings);
    }
    Some(value.clone())
}

fn lint_reasoning(value: &Value, scope: &str, warnings: &mut Vec<String>) {
    let Some(object) = value.as_object() else {
        warnings.push(format!(
            "ignored {scope} compat reasoning because it is not an object"
        ));
        return;
    };
    let rscope = format!("{scope} compat reasoning");
    warn_unknown(
        object.keys(),
        &["request_format", "effort_map", "capture"],
        &rscope,
        warnings,
    );
    redacted_choice_field(
        object,
        "request_format",
        &rscope,
        REASONING_FORMATS,
        warnings,
    );
    redacted_choice_field(object, "capture", &rscope, REASONING_CAPTURES, warnings);
    let Some(effort_map) = object.get("effort_map") else {
        return;
    };
    let Some(effort_map) = effort_map.as_object() else {
        warnings.push(format!(
            "ignored {rscope} effort_map because it is not an object"
        ));
        return;
    };
    for (level, value) in effort_map {
        if !matches!(
            level.as_str(),
            "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
        ) {
            warnings.push(format!("ignored {rscope} effort_map level `{level}` because it is not a known Euler reasoning level"));
        } else if !value.is_string() {
            warnings.push(format!(
                "ignored {rscope} effort_map level `{level}` because value is not a string"
            ));
        }
    }
}

fn valid_version(version: Option<&Value>, warnings: &mut Vec<String>) -> bool {
    match version {
        None => true,
        Some(value) if value.as_u64() == Some(1) => true,
        Some(_) => {
            warnings.push("ignored providers.json because version is not 1".to_owned());
            false
        }
    }
}

fn valid_model_id(id: &str, scope: &str, warnings: &mut Vec<String>) -> Option<String> {
    if id.is_empty() || id.trim() != id || id.contains("::") {
        warnings.push(format!(
            "ignored {scope} because model id is empty, padded, or contains `::`"
        ));
        None
    } else {
        Some(id.to_owned())
    }
}

fn object_value<'a>(
    value: &'a Value,
    scope: &str,
    warnings: &mut Vec<String>,
) -> Option<&'a Map<String, Value>> {
    value.as_object().or_else(|| {
        warnings.push(format!("ignored {scope} because it is not an object"));
        None
    })
}

fn string_field<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    scope: &str,
    warnings: &mut Vec<String>,
) -> Option<&'a str> {
    object
        .get(field)
        .and_then(|value| string_value(value, &format!("{scope} {field}"), warnings))
}

fn string_value<'a>(value: &'a Value, scope: &str, warnings: &mut Vec<String>) -> Option<&'a str> {
    value.as_str().or_else(|| {
        warnings.push(format!("ignored {scope} because it is not a string"));
        None
    })
}

fn positive_u64(
    object: &Map<String, Value>,
    field: &str,
    scope: &str,
    warnings: &mut Vec<String>,
) -> Option<u64> {
    let value = object.get(field)?;
    match value.as_u64() {
        Some(number) if number > 0 => Some(number),
        _ => {
            warnings.push(format!(
                "ignored {scope} {field} because it must be a positive JSON integer"
            ));
            None
        }
    }
}

fn bool_field(
    object: &Map<String, Value>,
    field: &str,
    scope: &str,
    warnings: &mut Vec<String>,
) -> Option<bool> {
    let value = object.get(field)?;
    value.as_bool().or_else(|| {
        warnings.push(format!(
            "ignored {scope} {field} because it must be a boolean"
        ));
        None
    })
}

fn choice_field(
    object: &Map<String, Value>,
    field: &str,
    scope: &str,
    allowed: &[&str],
    warnings: &mut Vec<String>,
) -> Option<String> {
    choice_field_with_policy(object, field, scope, allowed, warnings, true)
}

fn redacted_choice_field(
    object: &Map<String, Value>,
    field: &str,
    scope: &str,
    allowed: &[&str],
    warnings: &mut Vec<String>,
) -> Option<String> {
    choice_field_with_policy(object, field, scope, allowed, warnings, false)
}

fn choice_field_with_policy(
    object: &Map<String, Value>,
    field: &str,
    scope: &str,
    allowed: &[&str],
    warnings: &mut Vec<String>,
    show_unsupported_value: bool,
) -> Option<String> {
    let value = object.get(field)?;
    let Some(value) = value.as_str() else {
        warnings.push(format!(
            "ignored {scope} {field} because it is not a string"
        ));
        return None;
    };
    if allowed.contains(&value) {
        Some(value.to_owned())
    } else if show_unsupported_value {
        warnings.push(format!(
            "ignored {scope} {field} `{value}` because it is not supported"
        ));
        None
    } else {
        warnings.push(format!(
            "ignored {scope} {field} because it is not supported"
        ));
        None
    }
}

fn warn_unknown<'a>(
    fields: impl Iterator<Item = &'a String>,
    allowed: &[&str],
    scope: &str,
    warnings: &mut Vec<String>,
) {
    for field in fields {
        if !allowed.contains(&field.as_str()) {
            warnings.push(format!(
                "ignored unknown {scope} field `{field}` in providers.json"
            ));
        }
    }
}

const MODELS_FIX: &str =
    "list models as [{\"id\": \"<model id>\"}], or omit models and set \"default_model\": \"<model id>\"";
const MAX_TOKEN_FIELDS: &[&str] = &["max_completion_tokens", "max_tokens"];
const REASONING_FORMATS: &[&str] = &[
    "openai_reasoning_effort",
    "openrouter_reasoning",
    "zai_enable_thinking",
    "qwen_enable_thinking",
];
const REASONING_CAPTURES: &[&str] = &[
    "none",
    "counts_only",
    "readable_or_summary",
    "opaque_only",
    "readable_and_opaque",
];
const COMPAT_FIELDS: &[&str] = &[
    "supports_developer_role",
    "supports_stream_usage",
    "max_tokens_field",
    "requires_tool_result_name",
    "requires_assistant_after_tool_result",
    "supports_strict_tools",
    "reasoning",
];
