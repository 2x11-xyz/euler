//! Custom provider config parsing for `~/.euler/providers.json`.

use crate::catalog::canonical_provider_id;
use serde::de::{Deserializer, Error, MapAccess, Visitor};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApiFamily {
    OpenAiChatCompletions,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CustomModelConfig {
    pub id: String,
    pub display_name: String,
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
    choice_field(object, "api_family", &scope, API_FAMILIES, warnings)?;
    let api_family = ApiFamily::OpenAiChatCompletions;
    let base_url = required_base_url(object, &scope, warnings)?;
    let auth_header = bool_field(object, "auth_header", &scope, warnings).unwrap_or(false);
    let headers = parse_headers(object.get("headers"), &scope, auth_header, warnings)?;
    let models = parse_models(object.get("models"), &scope, warnings)?;
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
                "ignored {scope} because header `{name}` is owned by the OpenAI Chat Completions adapter"
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
    value: Option<&Value>,
    scope: &str,
    warnings: &mut Vec<String>,
) -> Option<BTreeMap<String, CustomModelConfig>> {
    let Some(models) = value.and_then(Value::as_array) else {
        warnings.push(format!(
            "ignored {scope} because models is missing or not an array"
        ));
        return None;
    };
    let mut parsed = BTreeMap::new();
    for (index, value) in models.iter().enumerate() {
        if let Some(model) = parse_model(value, &format!("{scope} model #{index}"), warnings) {
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
        warnings.push(format!("ignored {scope} because it has no valid models"));
        return None;
    }
    Some(parsed)
}

fn parse_model(
    value: &Value,
    scope: &str,
    warnings: &mut Vec<String>,
) -> Option<CustomModelConfig> {
    let object = object_value(value, scope, warnings)?;
    warn_unknown(
        object.keys(),
        &[
            "id",
            "display_name",
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
    Some(CustomModelConfig {
        display_name: string_field(object, "display_name", scope, warnings)
            .unwrap_or(&id)
            .to_owned(),
        context_window_tokens: positive_u64(object, "context_window_tokens", scope, warnings),
        max_output_tokens: positive_u64(object, "max_output_tokens", scope, warnings),
        supports_tools: bool_field(object, "supports_tools", scope, warnings),
        supports_reasoning: bool_field(object, "supports_reasoning", scope, warnings),
        compat: parse_compat(object.get("compat"), scope, warnings),
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

const API_FAMILIES: &[&str] = &["openai_chat_completions"];
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
