use super::ReasoningEffort;
use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub const EMBEDDED_CATALOG_JSON: &str = include_str!("../../catalog/catalog-v1.json");
pub const EMBEDDED_MANIFEST_JSON: &str = include_str!("../../catalog/manifest-v1.json");

const CATALOG_SCHEMA_VERSION: u64 = 1;
const MAX_PROVIDER_COUNT: usize = 32;
const MAX_MODEL_COUNT: usize = 10_000;
const MAX_MODEL_ID_BYTES: usize = 256;
const MAX_TOKEN_LIMIT: u64 = 20_000_000;
const EXPECTED_PROVIDER_IDS: [&str; 5] = [
    super::ANTHROPIC_PROVIDER_ID,
    super::CHATGPT_PROVIDER_ID,
    super::OPENAI_PROVIDER_ID,
    super::OPENROUTER_PROVIDER_ID,
    super::XAI_PROVIDER_ID,
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfficialCatalogError(String);

impl OfficialCatalogError {
    pub(super) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for OfficialCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for OfficialCatalogError {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OfficialCatalog {
    schema_version: u64,
    #[serde(deserialize_with = "deserialize_unique_providers")]
    pub(super) providers: BTreeMap<String, OfficialProvider>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OfficialProvider {
    pub(super) id: String,
    pub(super) display_name: String,
    pub(super) default_model: String,
    pub(super) aliases: Vec<String>,
    pub(super) models: Vec<OfficialModel>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(super) enum OfficialModelStatus {
    Active,
    Deprecated,
    Removed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum OfficialReasoningEffort {
    Xsmall,
    Small,
    Medium,
    Large,
    Xlarge,
    Max,
}

impl OfficialReasoningEffort {
    fn rank(self) -> u8 {
        match self {
            Self::Xsmall => 0,
            Self::Small => 1,
            Self::Medium => 2,
            Self::Large => 3,
            Self::Xlarge => 4,
            Self::Max => 5,
        }
    }

    fn into_runtime(self) -> ReasoningEffort {
        match self {
            Self::Xsmall => ReasoningEffort::XSmall,
            Self::Small => ReasoningEffort::Small,
            Self::Medium => ReasoningEffort::Medium,
            Self::Large => ReasoningEffort::Large,
            Self::Xlarge => ReasoningEffort::XLarge,
            Self::Max => ReasoningEffort::Max,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OfficialModel {
    pub(super) id: String,
    pub(super) display_name: String,
    pub(super) status: OfficialModelStatus,
    pub(super) context_window_tokens: u64,
    pub(super) max_output_tokens: Option<u64>,
    pub(super) supports_tools: bool,
    pub(super) supports_reasoning: bool,
    reasoning_efforts: Vec<OfficialReasoningEffort>,
    pub(super) cost: Option<Value>,
}

impl OfficialModel {
    pub(super) fn reasoning_efforts(&self) -> Vec<ReasoningEffort> {
        self.reasoning_efforts
            .iter()
            .copied()
            .map(OfficialReasoningEffort::into_runtime)
            .collect()
    }
}

pub(super) fn parse(contents: &str) -> Result<OfficialCatalog, OfficialCatalogError> {
    let catalog: OfficialCatalog = serde_json::from_str(contents)
        .map_err(|error| OfficialCatalogError::new(format!("invalid catalog JSON: {error}")))?;
    catalog.validate()?;
    Ok(catalog)
}

impl OfficialCatalog {
    fn validate(&self) -> Result<(), OfficialCatalogError> {
        if self.schema_version != CATALOG_SCHEMA_VERSION {
            return Err(OfficialCatalogError::new(format!(
                "unsupported catalog schema version {}",
                self.schema_version
            )));
        }
        if self.providers.is_empty() || self.providers.len() > MAX_PROVIDER_COUNT {
            return Err(OfficialCatalogError::new(
                "catalog provider count is out of bounds",
            ));
        }
        let actual_ids = self
            .providers
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        if actual_ids != EXPECTED_PROVIDER_IDS {
            return Err(OfficialCatalogError::new(format!(
                "catalog providers do not exactly match Euler adapters: {}",
                actual_ids.join(", ")
            )));
        }
        for (provider_id, provider) in &self.providers {
            provider.validate(provider_id)?;
        }
        Ok(())
    }
}

impl OfficialProvider {
    fn validate(&self, provider_key: &str) -> Result<(), OfficialCatalogError> {
        validate_provider_id(provider_key, "provider key")?;
        if self.id != provider_key {
            return Err(OfficialCatalogError::new(format!(
                "provider `{provider_key}` id does not match its key"
            )));
        }
        validate_display_name(
            &self.display_name,
            128,
            &format!("provider `{provider_key}`"),
        )?;
        validate_model_id(
            &self.default_model,
            &format!("provider `{provider_key}` default_model"),
        )?;
        if self.aliases.len() > 64 {
            return Err(OfficialCatalogError::new(format!(
                "provider `{provider_key}` has too many aliases"
            )));
        }
        validate_sorted_aliases(provider_key, &self.aliases)?;
        if self.models.is_empty() || self.models.len() > MAX_MODEL_COUNT {
            return Err(OfficialCatalogError::new(format!(
                "provider `{provider_key}` model count is out of bounds"
            )));
        }
        let mut previous_id: Option<&str> = None;
        for model in &self.models {
            model.validate(provider_key)?;
            if previous_id.is_some_and(|previous| previous >= model.id.as_str()) {
                return Err(OfficialCatalogError::new(format!(
                    "provider `{provider_key}` models are not uniquely sorted by id"
                )));
            }
            previous_id = Some(&model.id);
        }
        let model_ids = self
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<BTreeSet<_>>();
        if self
            .aliases
            .iter()
            .any(|alias| model_ids.contains(alias.as_str()))
        {
            return Err(OfficialCatalogError::new(format!(
                "provider `{provider_key}` aliases duplicate model ids"
            )));
        }
        let Some(default) = self
            .models
            .iter()
            .find(|model| model.id == self.default_model)
        else {
            return Err(OfficialCatalogError::new(format!(
                "provider `{provider_key}` default model is absent"
            )));
        };
        if default.status != OfficialModelStatus::Active {
            return Err(OfficialCatalogError::new(format!(
                "provider `{provider_key}` default model is not active"
            )));
        }
        Ok(())
    }
}

impl OfficialModel {
    fn validate(&self, provider_id: &str) -> Result<(), OfficialCatalogError> {
        let scope = format!("provider `{provider_id}` model `{}`", self.id);
        validate_model_id(&self.id, &scope)?;
        validate_display_name(&self.display_name, 256, &scope)?;
        validate_token_limit(self.context_window_tokens, "context_window_tokens", &scope)?;
        if let Some(limit) = self.max_output_tokens {
            validate_token_limit(limit, "max_output_tokens", &scope)?;
        }
        if !self.supports_tools {
            return Err(OfficialCatalogError::new(format!(
                "{scope} does not support required tool use"
            )));
        }
        if self.supports_reasoning == self.reasoning_efforts.is_empty() {
            return Err(OfficialCatalogError::new(format!(
                "{scope} reasoning support and effort list disagree"
            )));
        }
        let mut previous_rank: Option<u8> = None;
        for effort in &self.reasoning_efforts {
            let rank = effort.rank();
            if previous_rank.is_some_and(|previous| previous >= rank) {
                return Err(OfficialCatalogError::new(format!(
                    "{scope} reasoning efforts are not uniquely ordered"
                )));
            }
            previous_rank = Some(rank);
        }
        if let Some(cost) = &self.cost {
            super::parse_official_model_cost(cost, provider_id, &self.id)?;
        }
        Ok(())
    }
}

pub(super) fn embedded_release_id() -> String {
    serde_json::from_str::<Value>(EMBEDDED_MANIFEST_JSON)
        .ok()
        .and_then(|manifest| manifest.get("release_id")?.as_str().map(str::to_owned))
        .expect("packaged provider catalog manifest must contain release_id")
}

fn validate_provider_id(value: &str, scope: &str) -> Result<(), OfficialCatalogError> {
    let mut chars = value.chars();
    let valid = value.len() <= 64
        && chars
            .next()
            .is_some_and(|character| character.is_ascii_lowercase())
        && chars.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        });
    if valid {
        Ok(())
    } else {
        Err(OfficialCatalogError::new(format!("{scope} is invalid")))
    }
}

fn validate_model_id(value: &str, scope: &str) -> Result<(), OfficialCatalogError> {
    if value.is_empty()
        || value.len() > MAX_MODEL_ID_BYTES
        || value.contains("::")
        || value.chars().any(char::is_whitespace)
    {
        Err(OfficialCatalogError::new(format!(
            "{scope} has an invalid id"
        )))
    } else {
        Ok(())
    }
}

fn validate_display_name(
    value: &str,
    maximum_characters: usize,
    scope: &str,
) -> Result<(), OfficialCatalogError> {
    let length = value.chars().count();
    if length == 0 || length > maximum_characters {
        Err(OfficialCatalogError::new(format!(
            "{scope} has an invalid display_name"
        )))
    } else {
        Ok(())
    }
}

fn validate_token_limit(value: u64, field: &str, scope: &str) -> Result<(), OfficialCatalogError> {
    if value == 0 || value > MAX_TOKEN_LIMIT {
        Err(OfficialCatalogError::new(format!(
            "{scope} {field} is out of bounds"
        )))
    } else {
        Ok(())
    }
}

fn validate_sorted_aliases(
    provider_id: &str,
    aliases: &[String],
) -> Result<(), OfficialCatalogError> {
    let mut previous: Option<&str> = None;
    for alias in aliases {
        validate_model_id(alias, &format!("provider `{provider_id}` alias"))?;
        if previous.is_some_and(|value| value >= alias.as_str()) {
            return Err(OfficialCatalogError::new(format!(
                "provider `{provider_id}` aliases are not uniquely sorted"
            )));
        }
        previous = Some(alias);
    }
    Ok(())
}

fn deserialize_unique_providers<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, OfficialProvider>, D::Error>
where
    D: Deserializer<'de>,
{
    struct UniqueProviderMap;

    impl<'de> Visitor<'de> for UniqueProviderMap {
        type Value = BTreeMap<String, OfficialProvider>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object with unique provider keys")
        }

        fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut providers = BTreeMap::new();
            while let Some((key, provider)) = access.next_entry::<String, OfficialProvider>()? {
                if providers.insert(key.clone(), provider).is_some() {
                    return Err(A::Error::custom(format!("duplicate provider key `{key}`")));
                }
            }
            Ok(providers)
        }
    }

    deserializer.deserialize_map(UniqueProviderMap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_is_strictly_valid() {
        let catalog = parse(EMBEDDED_CATALOG_JSON).expect("embedded catalog");
        assert_eq!(catalog.providers.len(), EXPECTED_PROVIDER_IDS.len());
    }

    #[test]
    fn unknown_transport_field_is_rejected() {
        let tampered = EMBEDDED_CATALOG_JSON.replacen(
            "\"display_name\": \"Anthropic\"",
            "\"display_name\": \"Anthropic\", \"base_url\": \"https://example.test\"",
            1,
        );
        let error = parse(&tampered).expect_err("transport field must fail");
        assert!(error.to_string().contains("unknown field `base_url`"));
    }

    #[test]
    fn duplicate_provider_key_is_rejected() {
        let duplicate = r#"{
          "schema_version": 1,
          "providers": {
            "anthropic": {
              "id": "anthropic", "display_name": "Anthropic",
              "default_model": "model", "aliases": [], "models": []
            },
            "anthropic": {
              "id": "anthropic", "display_name": "Anthropic",
              "default_model": "model", "aliases": [], "models": []
            }
          }
        }"#;
        let error = parse(duplicate).expect_err("duplicate provider must fail");
        assert!(error.to_string().contains("duplicate provider key"));
    }

    #[test]
    fn removed_models_validate_but_leave_runtime_membership() {
        let removed =
            EMBEDDED_CATALOG_JSON.replacen("\"status\": \"active\"", "\"status\": \"removed\"", 1);
        let catalog = super::super::MergedModelCatalog::from_official_json(&removed)
            .expect("catalog with removed lifecycle record");
        assert!(catalog
            .provider(super::super::ANTHROPIC_PROVIDER_ID)
            .expect("anthropic")
            .models()
            .all(|model| model.id() != "claude-fable-5"));
    }

    #[test]
    fn official_price_metadata_is_strict_and_resolves_with_release_identity() {
        let mut document: Value = serde_json::from_str(EMBEDDED_CATALOG_JSON).expect("catalog");
        document["providers"]["anthropic"]["models"][0]["cost"] = serde_json::json!({
            "input": 10,
            "output": 50,
            "cache_read": 1,
            "cache_write_5m": 12.5,
            "cache_write_1h": 20
        });
        let catalog = super::super::MergedModelCatalog::from_official_json(
            &serde_json::to_string(&document).expect("json"),
        )
        .expect("official price")
        .with_official_release_id("catalog-v1-test");

        let resolved = catalog
            .resolved_model_cost("anthropic", "claude-fable-5")
            .expect("resolved price");
        assert_eq!(resolved.cost.rates.input, 10_000_000);
        assert!(matches!(
            resolved.source,
            super::super::ModelCostSource::Official { ref release_id }
                if release_id == "catalog-v1-test"
        ));
    }

    #[test]
    fn malformed_official_price_rejects_the_entire_catalog() {
        let mut document: Value = serde_json::from_str(EMBEDDED_CATALOG_JSON).expect("catalog");
        document["providers"]["anthropic"]["models"][0]["cost"] =
            serde_json::json!({"input": 0.1234567, "output": 50});

        let error = parse(&serde_json::to_string(&document).expect("json"))
            .expect_err("excess precision must fail closed");

        assert!(error.to_string().contains("at most six decimal places"));
    }
}
