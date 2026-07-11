use anyhow::Result;
use euler_provider::catalog::{MergedModelCatalog, ModelDescriptor};
use euler_provider::provider_config::{
    ApiFamily, CustomModelConfig, CustomProviderConfig, ProviderConfigRegistry,
};
use serde_json::{json, Map, Value};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const MODEL_CATALOG_FILE: &str = "models.json";

pub(crate) struct ModelCatalogLoad {
    pub(crate) catalog: MergedModelCatalog,
    pub(crate) warnings: Vec<String>,
}

pub(crate) fn default_model_catalog_path() -> Option<PathBuf> {
    model_catalog_path_from_home_vars(std::env::var_os("HOME"), std::env::var_os("USERPROFILE"))
}

fn model_catalog_path_from_home_vars(
    home: Option<OsString>,
    user_profile: Option<OsString>,
) -> Option<PathBuf> {
    home.or(user_profile)
        .map(PathBuf::from)
        .map(|home| home.join(".euler").join(MODEL_CATALOG_FILE))
}

pub(crate) fn load_model_catalog(path: Option<&Path>) -> ModelCatalogLoad {
    let Some(path) = path else {
        return ModelCatalogLoad {
            catalog: MergedModelCatalog::built_in(),
            warnings: Vec::new(),
        };
    };
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ModelCatalogLoad {
                catalog: MergedModelCatalog::built_in(),
                warnings: Vec::new(),
            };
        }
        Err(error) => {
            return ModelCatalogLoad {
                catalog: MergedModelCatalog::built_in(),
                warnings: vec![format!(
                    "could not read {}: {error}; using built-in model catalog",
                    path.display()
                )],
            };
        }
    };
    let (catalog, warnings) = MergedModelCatalog::with_local_json(&contents);
    ModelCatalogLoad { catalog, warnings }
}

pub(crate) fn print_model_catalog(
    path: Option<&Path>,
    provider_config_path: Option<&Path>,
    mut stdout: impl Write,
    mut stderr: impl Write,
) -> Result<()> {
    let load = load_model_catalog(path);
    for warning in &load.warnings {
        writeln!(stderr, "warning: ignored model catalog: {warning}")?;
    }
    let provider_load = crate::provider_config_runtime::load_provider_config(provider_config_path);
    for warning in &provider_load.warnings {
        writeln!(stderr, "warning: ignored provider config: {warning}")?;
    }
    write_model_catalog_json_with_custom(&load.catalog, &provider_load.registry, &mut stdout)
}

pub(crate) fn write_model_catalog_json_with_custom(
    catalog: &MergedModelCatalog,
    custom_providers: &ProviderConfigRegistry,
    mut output: impl Write,
) -> Result<()> {
    let mut custom = custom_providers.providers().collect::<Vec<_>>();
    custom.sort_by(|left, right| left.id.cmp(&right.id));
    let providers = catalog
        .providers()
        .map(|provider| {
            let models = provider
                .models()
                .map(|model| model_json(model, model.id() == provider.default_model()))
                .collect::<Vec<_>>();
            json!({
                "id": provider.id(),
                "display_name": provider.display_name(),
                "default_model": provider.default_model(),
                "auth_file_supported": provider.auth_file_supported(),
                "models": models,
            })
        })
        .chain(custom.into_iter().map(custom_provider_json))
        .collect::<Vec<_>>();
    serde_json::to_writer_pretty(&mut output, &json!({ "providers": providers }))?;
    writeln!(output)?;
    Ok(())
}

fn model_json(model: &ModelDescriptor, is_default: bool) -> Value {
    let mut object = Map::new();
    object.insert("id".to_owned(), json!(model.id()));
    object.insert("display_name".to_owned(), json!(model.display_name()));
    object.insert("source".to_owned(), json!(model.source().as_str()));
    object.insert("default".to_owned(), json!(is_default));
    if let Some(value) = model.context_window_tokens() {
        object.insert("context_window_tokens".to_owned(), json!(value));
    }
    if let Some(value) = model.max_output_tokens() {
        object.insert("max_output_tokens".to_owned(), json!(value));
    }
    if let Some(value) = model.supports_tools() {
        object.insert("supports_tools".to_owned(), json!(value));
    }
    if let Some(value) = model.supports_reasoning() {
        object.insert("supports_reasoning".to_owned(), json!(value));
    }
    Value::Object(object)
}

fn custom_provider_json(provider: &CustomProviderConfig) -> Value {
    let default_model = provider.default_model.as_deref();
    let mut object = Map::new();
    object.insert("id".to_owned(), json!(provider.id.as_str()));
    object.insert("display_name".to_owned(), json!(provider.id.as_str()));
    object.insert("source".to_owned(), json!("custom"));
    object.insert(
        "api_family".to_owned(),
        json!(api_family(provider.api_family)),
    );
    object.insert("default_model".to_owned(), json!(default_model));
    if let Some(error) = &provider.default_model_error {
        object.insert("default_model_error".to_owned(), json!(error));
    }
    object.insert("auth_file_supported".to_owned(), json!(false));
    object.insert("auth".to_owned(), auth_json(provider));
    object.insert(
        "headers".to_owned(),
        Value::Array(sorted_headers(provider).map(header_json).collect()),
    );
    object.insert("diagnostics".to_owned(), diagnostics_json(provider));
    object.insert(
        "models".to_owned(),
        Value::Array(
            sorted_custom_models(provider)
                .map(|model| custom_model_json(model, default_model == Some(model.id.as_str())))
                .collect(),
        ),
    );
    Value::Object(object)
}

fn sorted_headers(provider: &CustomProviderConfig) -> impl Iterator<Item = (&String, &String)> {
    let mut headers = provider.headers.iter().collect::<Vec<_>>();
    headers.sort_by_key(|(name, _)| *name);
    headers.into_iter()
}

fn header_json((name, value): (&String, &String)) -> Value {
    json!({
        "name": name,
        "status": secret_status(value),
    })
}

fn sorted_custom_models(
    provider: &CustomProviderConfig,
) -> impl Iterator<Item = &CustomModelConfig> {
    let mut models = provider.models.values().collect::<Vec<_>>();
    models.sort_by(|left, right| left.id.cmp(&right.id));
    models.into_iter()
}

fn custom_model_json(model: &CustomModelConfig, is_default: bool) -> Value {
    let mut object = Map::new();
    object.insert("id".to_owned(), json!(model.id.as_str()));
    object.insert(
        "display_name".to_owned(),
        json!(model.display_name.as_str()),
    );
    object.insert("source".to_owned(), json!("custom"));
    object.insert("default".to_owned(), json!(is_default));
    if let Some(value) = model.context_window_tokens {
        object.insert("context_window_tokens".to_owned(), json!(value));
    }
    if let Some(value) = model.max_output_tokens {
        object.insert("max_output_tokens".to_owned(), json!(value));
    }
    if let Some(value) = model.supports_tools {
        object.insert("supports_tools".to_owned(), json!(value));
    }
    if let Some(value) = model.supports_reasoning {
        object.insert("supports_reasoning".to_owned(), json!(value));
    }
    Value::Object(object)
}

fn auth_json(provider: &CustomProviderConfig) -> Value {
    json!({
        "auth_header": provider.auth_header,
        "api_key": optional_secret_status(provider.api_key.as_deref()),
        "configured_headers": provider.headers.len(),
    })
}

fn diagnostics_json(provider: &CustomProviderConfig) -> Value {
    let mut diagnostics = Vec::new();
    if let Some(error) = &provider.default_model_error {
        diagnostics.push(json!({
            "kind": "default_model",
            "severity": "warning",
            "message": error,
        }));
    }
    Value::Array(diagnostics)
}

fn secret_status(value: &str) -> &'static str {
    if value.trim().is_empty() {
        "empty"
    } else {
        "configured"
    }
}

fn optional_secret_status(value: Option<&str>) -> &'static str {
    match value {
        None => "missing",
        Some(value) => secret_status(value),
    }
}

fn api_family(api_family: ApiFamily) -> &'static str {
    match api_family {
        ApiFamily::OpenAiChatCompletions => "openai_chat_completions",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_model_catalog_is_silent_built_in_fallback() {
        let temp = tempfile::tempdir().expect("temp dir");
        let load = load_model_catalog(Some(&temp.path().join("missing.json")));

        assert!(load.warnings.is_empty());
        assert_eq!(
            load.catalog.default_model_for_provider("fixture"),
            Some(euler_provider::catalog::DEFAULT_FIXTURE_MODEL)
        );
    }

    #[test]
    fn unreadable_model_catalog_path_warns_and_falls_back() {
        let temp = tempfile::tempdir().expect("temp dir");
        let load = load_model_catalog(Some(temp.path()));

        assert_eq!(
            load.catalog.default_model_for_provider("fixture"),
            Some(euler_provider::catalog::DEFAULT_FIXTURE_MODEL)
        );
        assert!(load
            .warnings
            .iter()
            .any(|message| message.contains("could not read")));
    }

    #[test]
    fn print_model_catalog_keeps_warnings_on_stderr_and_json_on_stdout() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("models.json");
        fs::write(
            &path,
            r#"{
              "version": 1,
                "providers": {
                "api_key": "SHOULD_NOT_APPEAR",
                "missing": { "default_model": "ignored" },
                "chatgpt": {
                  "default_model": "gpt-custom",
                  "models": [
                    {
                      "id": "gpt-custom",
                      "display_name": "GPT Custom",
                      "context_window_tokens": 128000,
                      "max_output_tokens": "8192",
                      "supports_tools": true,
                      "supports_reasoning": "true",
                      "token": "SHOULD_NOT_APPEAR"
                    }
                  ]
                }
              }
            }"#,
        )
        .expect("write catalog");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        print_model_catalog(Some(&path), None, &mut stdout, &mut stderr).expect("print catalog");

        let stdout = String::from_utf8(stdout).expect("stdout utf8");
        let stderr = String::from_utf8(stderr).expect("stderr utf8");
        let value: serde_json::Value = serde_json::from_str(&stdout).expect("json stdout");
        assert!(stderr.contains("unknown provider `missing`"));
        assert!(stderr.contains("max_output_tokens"));
        assert!(stderr.contains("positive JSON integer"));
        assert!(stderr.contains("supports_reasoning"));
        assert!(stderr.contains("boolean"));
        assert!(!stdout.contains("unknown provider"));
        assert!(!stdout.contains("positive JSON integer"));
        assert!(!stdout.contains("SHOULD_NOT_APPEAR"));
        assert!(!stderr.contains("SHOULD_NOT_APPEAR"));
        assert_eq!(
            value["providers"]
                .as_array()
                .expect("providers")
                .iter()
                .find(|provider| provider["id"] == "chatgpt")
                .expect("chatgpt")["default_model"],
            "gpt-custom"
        );
        let model = value["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .find(|provider| provider["id"] == "chatgpt")
            .expect("chatgpt")["models"]
            .as_array()
            .expect("models")
            .iter()
            .find(|model| model["id"] == "gpt-custom")
            .expect("gpt custom");
        assert_eq!(model["default"], true);
        assert_eq!(model["context_window_tokens"], 128000);
        assert_eq!(model["supports_tools"], true);
        assert!(model.get("max_output_tokens").is_none());
        assert!(model.get("supports_reasoning").is_none());
    }

    #[test]
    fn write_model_catalog_json_omits_absent_metadata_keys() {
        let (catalog, warnings) = MergedModelCatalog::with_local_json(
            r#"{
              "version": 1,
              "providers": {
                "openai": {
                  "models": [
                    { "id": "gpt-no-metadata", "display_name": "No Metadata" }
                  ]
                }
              }
            }"#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut output = Vec::new();

        write_model_catalog_json_with_custom(
            &catalog,
            &ProviderConfigRegistry::default(),
            &mut output,
        )
        .expect("write");

        let value: serde_json::Value = serde_json::from_slice(&output).expect("model catalog json");
        let model = value["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .find(|provider| provider["id"] == "openai")
            .expect("openai")["models"]
            .as_array()
            .expect("models")
            .iter()
            .find(|model| model["id"] == "gpt-no-metadata")
            .expect("model");
        assert!(model.get("context_window_tokens").is_none());
        assert!(model.get("max_output_tokens").is_none());
        assert!(model.get("supports_tools").is_none());
        assert!(model.get("supports_reasoning").is_none());
    }

    #[test]
    fn write_model_catalog_json_includes_all_present_metadata() {
        let (catalog, warnings) = MergedModelCatalog::with_local_json(
            r#"{
              "version": 1,
              "providers": {
                "anthropic": {
                  "models": [{
                    "id": "claude-metadata",
                    "context_window_tokens": 200000,
                    "max_output_tokens": 8192,
                    "supports_tools": true,
                    "supports_reasoning": true
                  }]
                }
              }
            }"#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut output = Vec::new();

        write_model_catalog_json_with_custom(
            &catalog,
            &ProviderConfigRegistry::default(),
            &mut output,
        )
        .expect("write");

        let value: serde_json::Value = serde_json::from_slice(&output).expect("model catalog json");
        let model = value["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .find(|provider| provider["id"] == "anthropic")
            .expect("anthropic")["models"]
            .as_array()
            .expect("models")
            .iter()
            .find(|model| model["id"] == "claude-metadata")
            .expect("model");
        assert_eq!(model["default"], false);
        assert_eq!(model["context_window_tokens"], 200000);
        assert_eq!(model["max_output_tokens"], 8192);
        assert_eq!(model["supports_tools"], true);
        assert_eq!(model["supports_reasoning"], true);
    }

    #[test]
    fn write_model_catalog_json_is_deterministic() {
        let (catalog, warnings) = MergedModelCatalog::with_local_json(
            r#"{
              "version": 1,
              "providers": {
                "openrouter": {
                  "models": [{ "id": "z-model" }, { "id": "a-model" }]
                }
              }
            }"#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut first = Vec::new();
        let mut second = Vec::new();

        write_model_catalog_json_with_custom(
            &catalog,
            &ProviderConfigRegistry::default(),
            &mut first,
        )
        .expect("first");
        write_model_catalog_json_with_custom(
            &catalog,
            &ProviderConfigRegistry::default(),
            &mut second,
        )
        .expect("second");

        assert_eq!(first, second);
        let output = String::from_utf8(first).expect("utf8");
        let value: serde_json::Value = serde_json::from_str(&output).expect("json");
        let provider_ids = value["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .map(|provider| provider["id"].as_str().expect("id").to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            provider_ids,
            vec![
                "anthropic",
                "chatgpt",
                "fixture",
                "openai",
                "openrouter",
                "xai"
            ]
        );
        assert!(output.find("\"a-model\"") < output.find("\"z-model\""));
    }

    #[test]
    fn print_model_catalog_includes_custom_providers_without_secret_values() {
        let temp = tempfile::tempdir().expect("temp dir");
        let provider_path = temp.path().join("providers.json");
        fs::write(
            &provider_path,
            r#"{
              "version": 1,
              "providers": {
                "local-b": {
                  "api_family": "openai_chat_completions",
                  "base_url": "http://localhost:11434/v1",
                  "api_key": "$VERY_SECRET_ENV",
                  "auth_header": true,
                  "headers": {
                    "z-extra-secret": "!secret command",
                    "a-literal-secret": "literal-secret-value",
                    "m-empty-secret": ""
                  },
                  "default_model": "b-model",
                  "models": [
                    {
                      "id": "z-model",
                      "display_name": "Z Model"
                    },
                    {
                      "id": "b-model",
                      "display_name": "B Model",
                      "supports_tools": true,
                      "supports_reasoning": true,
                      "context_window_tokens": 4096,
                      "max_output_tokens": 1024
                    }
                  ]
                },
                "local-a": {
                  "api_family": "openai_chat_completions",
                  "base_url": "http://localhost:11434/v1",
                  "models": [{ "id": "a-model" }]
                }
              }
            }"#,
        )
        .expect("write providers");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        print_model_catalog(None, Some(&provider_path), &mut stdout, &mut stderr)
            .expect("print catalog");

        let stdout = String::from_utf8(stdout).expect("stdout utf8");
        let stderr = String::from_utf8(stderr).expect("stderr utf8");
        let value: serde_json::Value = serde_json::from_str(&stdout).expect("json stdout");
        assert!(stderr.is_empty(), "{stderr}");
        assert_no_secret_material(&stdout);
        assert_no_secret_material(&stderr);

        let providers = value["providers"].as_array().expect("providers");
        assert_unique_provider_ids(providers);
        let provider_ids = providers
            .iter()
            .map(|provider| provider["id"].as_str().expect("id").to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            provider_ids,
            vec![
                "anthropic",
                "chatgpt",
                "fixture",
                "openai",
                "openrouter",
                "xai",
                "local-a",
                "local-b"
            ]
        );

        let local_a = provider_by_id(providers, "local-a");
        assert_eq!(local_a["source"], "custom");
        assert_eq!(local_a["default_model"], serde_json::Value::Null);
        assert_eq!(local_a["auth"]["api_key"], "missing");
        assert_eq!(local_a["auth"]["configured_headers"], 0);
        assert_eq!(
            local_a["models"][0]["source"],
            serde_json::Value::String("custom".to_owned())
        );

        let local_b = provider_by_id(providers, "local-b");
        assert_eq!(local_b["source"], "custom");
        assert_eq!(local_b["api_family"], "openai_chat_completions");
        assert_eq!(local_b["default_model"], "b-model");
        assert_eq!(local_b["auth_file_supported"], false);
        assert_eq!(local_b["auth"]["auth_header"], true);
        assert_eq!(local_b["auth"]["api_key"], "configured");
        assert_eq!(local_b["auth"]["configured_headers"], 3);
        assert_eq!(local_b["headers"][0]["name"], "a-literal-secret");
        assert_eq!(local_b["headers"][0]["status"], "configured");
        assert_eq!(local_b["headers"][1]["name"], "m-empty-secret");
        assert_eq!(local_b["headers"][1]["status"], "empty");
        assert_eq!(local_b["headers"][2]["name"], "z-extra-secret");
        assert_eq!(local_b["headers"][2]["status"], "configured");
        assert_eq!(local_b["models"][0]["id"], "b-model");
        assert_eq!(local_b["models"][0]["default"], true);
        assert_eq!(local_b["models"][0]["supports_tools"], true);
        assert_eq!(local_b["models"][0]["supports_reasoning"], true);
        assert_eq!(local_b["models"][0]["context_window_tokens"], 4096);
        assert_eq!(local_b["models"][0]["max_output_tokens"], 1024);
        assert_eq!(local_b["models"][1]["id"], "z-model");
        assert_eq!(local_b["models"][1]["default"], false);
    }

    #[test]
    fn print_model_catalog_lists_default_model_diagnostics_and_redacted_warnings() {
        let temp = tempfile::tempdir().expect("temp dir");
        let provider_path = temp.path().join("providers.json");
        fs::write(
            &provider_path,
            r#"{
              "providers": {
                "openai": {
                  "api_family": "openai_chat_completions",
                  "base_url": "http://localhost:11434/v1",
                  "api_key": "literal-secret-value",
                  "models": [{ "id": "ignored" }]
                },
                "local-openai": {
                  "api_family": "openai_chat_completions",
                  "base_url": "http://localhost:11434/v1",
                  "api_key": "",
                  "headers": { "x-extra-secret": "$RAW_SECRET_REF" },
                  "default_model": "missing-model",
                  "models": [{ "id": "available-model" }]
                }
              }
            }"#,
        )
        .expect("write providers");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        print_model_catalog(None, Some(&provider_path), &mut stdout, &mut stderr)
            .expect("print catalog");

        let stdout = String::from_utf8(stdout).expect("stdout utf8");
        let stderr = String::from_utf8(stderr).expect("stderr utf8");
        let value: serde_json::Value = serde_json::from_str(&stdout).expect("json stdout");
        assert!(stderr.contains("built-in provider ids"));
        assert_no_secret_material(&stdout);
        assert_no_secret_material(&stderr);

        let providers = value["providers"].as_array().expect("providers");
        assert_unique_provider_ids(providers);
        assert_eq!(
            providers
                .iter()
                .filter(|provider| provider["id"] == "openai")
                .count(),
            1
        );
        let local = provider_by_id(providers, "local-openai");
        assert_eq!(local["default_model"], serde_json::Value::Null);
        assert_eq!(
            local["default_model_error"],
            "providers.local-openai.default_model references unknown model `missing-model`"
        );
        assert_eq!(local["auth"]["api_key"], "empty");
        assert_eq!(local["headers"][0]["status"], "configured");
        assert_eq!(local["diagnostics"][0]["kind"], "default_model");
        assert_eq!(local["diagnostics"][0]["severity"], "warning");
        assert_eq!(local["models"][0]["default"], false);
    }

    #[test]
    fn print_model_catalog_keeps_malformed_provider_config_warning_on_stderr() {
        let temp = tempfile::tempdir().expect("temp dir");
        let provider_path = temp.path().join("providers.json");
        fs::write(&provider_path, "{").expect("write providers");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        print_model_catalog(None, Some(&provider_path), &mut stdout, &mut stderr)
            .expect("print catalog");

        let stdout = String::from_utf8(stdout).expect("stdout utf8");
        let stderr = String::from_utf8(stderr).expect("stderr utf8");
        let value: serde_json::Value = serde_json::from_str(&stdout).expect("json stdout");
        assert!(stderr.contains("providers.json is not valid JSON"));
        assert!(value["providers"].as_array().expect("providers").len() >= 5);
    }

    #[test]
    fn print_model_catalog_accepts_empty_provider_config() {
        let temp = tempfile::tempdir().expect("temp dir");
        let provider_path = temp.path().join("providers.json");
        fs::write(&provider_path, r#"{ "version": 1, "providers": {} }"#).expect("write providers");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        print_model_catalog(None, Some(&provider_path), &mut stdout, &mut stderr)
            .expect("print catalog");

        let stdout = String::from_utf8(stdout).expect("stdout utf8");
        let stderr = String::from_utf8(stderr).expect("stderr utf8");
        let value: serde_json::Value = serde_json::from_str(&stdout).expect("json stdout");
        assert!(stderr.is_empty(), "{stderr}");
        assert_eq!(value["providers"].as_array().expect("providers").len(), 6);
    }

    #[test]
    fn default_model_catalog_path_uses_home_then_user_profile() {
        let home = OsString::from("/tmp/home-catalog");
        let user_profile = OsString::from("C:\\Users\\euler");

        assert_eq!(
            model_catalog_path_from_home_vars(Some(home.clone()), Some(user_profile.clone())),
            Some(PathBuf::from("/tmp/home-catalog/.euler/models.json"))
        );
        assert_eq!(
            model_catalog_path_from_home_vars(None, Some(user_profile)),
            Some(
                PathBuf::from("C:\\Users\\euler")
                    .join(".euler")
                    .join("models.json")
            )
        );
        assert_eq!(model_catalog_path_from_home_vars(None, None), None);
    }

    fn provider_by_id<'a>(providers: &'a [serde_json::Value], id: &str) -> &'a serde_json::Value {
        providers
            .iter()
            .find(|provider| provider["id"] == id)
            .unwrap_or_else(|| panic!("provider {id}"))
    }

    fn assert_unique_provider_ids(providers: &[serde_json::Value]) {
        let mut ids = std::collections::BTreeSet::new();
        for provider in providers {
            let id = provider["id"].as_str().expect("id");
            assert!(ids.insert(id), "duplicate provider id {id}");
        }
    }

    fn assert_no_secret_material(output: &str) {
        for secret in [
            "$VERY_SECRET_ENV",
            "!secret command",
            "literal-secret-value",
            "$RAW_SECRET_REF",
        ] {
            assert!(!output.contains(secret), "leaked {secret} in {output}");
        }
    }
}
