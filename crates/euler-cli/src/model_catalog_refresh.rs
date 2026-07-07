use anyhow::{anyhow, Result};
use euler_provider::catalog::{
    DEFAULT_ANTHROPIC_MODEL, DEFAULT_CHATGPT_MODEL, DEFAULT_OPENAI_MODEL, DEFAULT_OPENROUTER_MODEL,
};
use serde_json::{json, Map, Value};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const GENERATED_BY: &str = "euler models refresh";
const MAX_MODELS_DEV_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) fn refresh_model_catalog(
    path: Option<&Path>,
    force: bool,
    mut stdout: impl Write,
    mut stderr: impl Write,
) -> Result<()> {
    let path = path.ok_or_else(|| anyhow!("could not resolve ~/.euler/models.json"))?;
    let contents = fetch_models_dev_catalog()?;
    let (overlay, warnings) = translate_modelsdev_json(&contents).map_err(|error| {
        anyhow!("failed to parse {MODELS_DEV_URL}: {error}; models.json left untouched")
    })?;
    for warning in warnings {
        writeln!(stderr, "warning: {warning}")?;
    }
    write_generated_catalog(path, &overlay, force)?;
    writeln!(stdout, "wrote {}", path.display())?;
    Ok(())
}

fn fetch_models_dev_catalog() -> Result<String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build();
    let response = agent.get(MODELS_DEV_URL).call().map_err(|error| {
        anyhow!("failed to fetch {MODELS_DEV_URL}: {error}; models.json left untouched")
    })?;
    read_bounded_to_string(response.into_reader(), MAX_MODELS_DEV_BYTES, MODELS_DEV_URL)
}

fn read_bounded_to_string(reader: impl Read, max_bytes: u64, source_name: &str) -> Result<String> {
    let mut limited = reader.take(max_bytes.saturating_add(1));
    let mut contents = String::new();
    limited.read_to_string(&mut contents).map_err(|error| {
        anyhow!("failed to read {source_name}: {error}; models.json left untouched")
    })?;
    if contents.len() as u64 > max_bytes {
        return Err(anyhow!(
            "failed to read {source_name}: response exceeds {max_bytes} byte limit; models.json left untouched"
        ));
    }
    Ok(contents)
}

pub(crate) fn translate_modelsdev_json(contents: &str) -> Result<(Value, Vec<String>)> {
    let value: Value = serde_json::from_str(contents)
        .map_err(|error| anyhow!("models.dev catalog is not valid JSON: {error}"))?;
    let root = value
        .as_object()
        .ok_or_else(|| anyhow!("models.dev catalog root must be an object"))?;
    let mut warnings = Vec::new();
    let mut providers = Map::new();

    insert_provider(
        root,
        "anthropic",
        DEFAULT_ANTHROPIC_MODEL,
        &mut providers,
        &mut warnings,
    );
    insert_provider(
        root,
        "openai",
        DEFAULT_OPENAI_MODEL,
        &mut providers,
        &mut warnings,
    );
    if let Some(openai) = providers.remove("openai") {
        let mut chatgpt = openai.clone();
        chatgpt["default_model"] = Value::String(DEFAULT_CHATGPT_MODEL.to_owned());
        providers.insert("chatgpt".to_owned(), chatgpt);
        providers.insert("openai".to_owned(), openai);
    }
    insert_provider(
        root,
        "openrouter",
        DEFAULT_OPENROUTER_MODEL,
        &mut providers,
        &mut warnings,
    );

    Ok((
        json!({
            "version": 1,
            "generated_by": GENERATED_BY,
            "providers": providers,
        }),
        warnings,
    ))
}

fn insert_provider(
    root: &Map<String, Value>,
    provider_id: &str,
    default_model: &str,
    providers: &mut Map<String, Value>,
    warnings: &mut Vec<String>,
) {
    let Some(section) = root.get(provider_id) else {
        warnings.push(format!(
            "skipped missing models.dev provider `{provider_id}`"
        ));
        return;
    };
    let Some(models) = section.get("models").and_then(Value::as_object) else {
        warnings.push(format!(
            "skipped models.dev provider `{provider_id}` because models is missing or not an object"
        ));
        return;
    };
    let models = models
        .iter()
        .filter_map(|(key, model)| translate_model(provider_id, key, model, warnings))
        .collect::<Vec<_>>();
    providers.insert(
        provider_id.to_owned(),
        json!({
            "default_model": default_model,
            "models": models,
        }),
    );
}

fn translate_model(
    provider_id: &str,
    key: &str,
    model: &Value,
    warnings: &mut Vec<String>,
) -> Option<Value> {
    let scope = format!("models.dev provider `{provider_id}` model `{key}`");
    let Some(object) = model.as_object() else {
        warnings.push(format!("skipped {scope} because it is not an object"));
        return None;
    };
    match object.get("tool_call").and_then(Value::as_bool) {
        Some(true) => {}
        Some(false) => return None,
        None => {
            warnings.push(format!(
                "skipped {scope} because tool_call is missing or not a boolean"
            ));
            return None;
        }
    }
    let Some(id) = string_field(object, "id") else {
        warnings.push(format!(
            "skipped {scope} because id is missing or not a string"
        ));
        return None;
    };
    let Some(display_name) = string_field(object, "name") else {
        warnings.push(format!(
            "skipped {scope} because name is missing or not a string"
        ));
        return None;
    };
    let Some(supports_reasoning) = object.get("reasoning").and_then(Value::as_bool) else {
        warnings.push(format!(
            "skipped {scope} because reasoning is missing or not a boolean"
        ));
        return None;
    };
    let Some(limit) = object.get("limit").and_then(Value::as_object) else {
        warnings.push(format!(
            "skipped {scope} because limit is missing or not an object"
        ));
        return None;
    };
    let Some(context_window_tokens) = positive_u64(limit, "context") else {
        warnings.push(format!(
            "skipped {scope} because limit.context is missing or not a positive integer"
        ));
        return None;
    };
    let Some(max_output_tokens) = positive_u64(limit, "output") else {
        warnings.push(format!(
            "skipped {scope} because limit.output is missing or not a positive integer"
        ));
        return None;
    };

    // The models.dev entry already passed the tool_call filter, so the local
    // overlay can record tool support directly.
    Some(json!({
        "id": id,
        "display_name": display_name,
        "context_window_tokens": context_window_tokens,
        "max_output_tokens": max_output_tokens,
        "supports_tools": true,
        "supports_reasoning": supports_reasoning,
    }))
}

fn string_field<'a>(object: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn positive_u64(object: &Map<String, Value>, field: &str) -> Option<u64> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
}

pub(crate) fn write_generated_catalog(path: &Path, overlay: &Value, force: bool) -> Result<()> {
    let marked = if path.exists() && !force {
        existing_catalog_has_marker(path).map_err(|error| {
            anyhow!(
                "failed to read existing {}: {error}; models.json left untouched",
                path.display()
            )
        })?
    } else {
        true
    };
    if !marked {
        return Err(anyhow!(
            "{} already exists and was not generated by `euler models refresh`; pass --force to overwrite; models.json left untouched",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            anyhow!(
                "failed to write {}: {error}; models.json left untouched",
                path.display()
            )
        })?;
    }
    let mut bytes = serde_json::to_vec_pretty(overlay)?;
    bytes.push(b'\n');
    let temp_path = temp_path_for(path);
    fs::write(&temp_path, bytes).map_err(|error| {
        anyhow!(
            "failed to write {}: {error}; models.json left untouched",
            path.display()
        )
    })?;
    if let Err(error) = fs::rename(&temp_path, path) {
        // Portable tests cannot reliably force fs::rename to fail after a
        // successful temp-file write: Unix permissions, Windows replacement
        // rules, and sandbox mount behavior differ. Keep the production cleanup
        // best-effort and assert the deterministic temp path in other tests.
        let _ = fs::remove_file(&temp_path);
        return Err(anyhow!(
            "failed to write {}: {error}; models.json left untouched",
            path.display()
        ));
    }
    Ok(())
}

fn existing_catalog_has_marker(path: &Path) -> Result<bool> {
    let contents = fs::read_to_string(path)?;
    Ok(serde_json::from_str::<Value>(&contents)
        .ok()
        .and_then(|value| {
            value
                .get("generated_by")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some(GENERATED_BY))
}

fn temp_path_for(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("models.json")
        .to_owned();
    name.push_str(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
#[path = "model_catalog_refresh_tests.rs"]
mod tests;
