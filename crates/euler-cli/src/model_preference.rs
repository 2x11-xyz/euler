use crate::theme_catalog::ThemeChoice;
use anyhow::{anyhow, Result};
use serde_json::{Map, Value};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

const PREFERENCE_FILE: &str = "preferences.json";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelPreference {
    pub provider: String,
    pub model: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreferenceLoad {
    Loaded(ModelPreference),
    Missing,
    Ignored(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThemePreferenceLoad {
    Loaded(String),
    Missing,
    Ignored(String),
}

pub fn default_model_preference_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".euler").join(PREFERENCE_FILE))
}

pub fn load_model_preference(path: &Path) -> PreferenceLoad {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return PreferenceLoad::Missing,
        Err(error) => {
            return PreferenceLoad::Ignored(format!("could not read model preference: {error}"));
        }
    };
    preference_from_json(&contents)
}

pub fn save_model_preference(path: &Path, provider: &str, model: &str) -> Result<()> {
    let mut payload = read_preference_object_for_save(path)?;
    payload.insert("provider".to_owned(), Value::String(provider.to_owned()));
    payload.insert("model".to_owned(), Value::String(model.to_owned()));
    write_preference_object(path, payload)
}

pub fn save_model_preference_to_default(provider: &str, model: &str) -> Result<()> {
    let path = default_model_preference_path()
        .ok_or_else(|| anyhow!("HOME is not set; model preference path is unavailable"))?;
    save_model_preference(&path, provider, model)
}

pub fn load_theme_preference(path: &Path) -> ThemePreferenceLoad {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return ThemePreferenceLoad::Missing,
        Err(error) => {
            return ThemePreferenceLoad::Ignored(format!(
                "could not read theme preference: {error}"
            ));
        }
    };
    theme_preference_from_json(&contents)
}

pub fn save_theme_preference(path: &Path, theme: &str) -> Result<()> {
    let choice = ThemeChoice::parse(theme).ok_or_else(|| {
        anyhow!(
            "theme preference must be one of {}",
            ThemeChoice::format_canonical_ids("|")
        )
    })?;
    let mut payload = read_preference_object_for_save(path)?;
    payload.insert(
        "theme".to_owned(),
        Value::String(choice.as_str().to_owned()),
    );
    write_preference_object(path, payload)
}

fn preference_from_json(contents: &str) -> PreferenceLoad {
    let value: Value = match serde_json::from_str(contents) {
        Ok(value) => value,
        Err(error) => {
            return PreferenceLoad::Ignored(format!("malformed model preference: {error}"));
        }
    };
    let Some(object) = value.as_object() else {
        return PreferenceLoad::Ignored("malformed model preference: expected object".to_owned());
    };
    let Some(provider) = object.get("provider").and_then(Value::as_str) else {
        return PreferenceLoad::Ignored("malformed model preference: missing provider".to_owned());
    };
    let Some(model) = object.get("model").and_then(Value::as_str) else {
        return PreferenceLoad::Ignored("malformed model preference: missing model".to_owned());
    };
    if provider.is_empty() || model.is_empty() {
        return PreferenceLoad::Ignored(
            "malformed model preference: provider and model must be non-empty".to_owned(),
        );
    }
    PreferenceLoad::Loaded(ModelPreference {
        provider: provider.to_owned(),
        model: model.to_owned(),
    })
}

fn theme_preference_from_json(contents: &str) -> ThemePreferenceLoad {
    let value: Value = match serde_json::from_str(contents) {
        Ok(value) => value,
        Err(error) => {
            return ThemePreferenceLoad::Ignored(format!("malformed theme preference: {error}"));
        }
    };
    let Some(object) = value.as_object() else {
        return ThemePreferenceLoad::Ignored(
            "malformed theme preference: expected object".to_owned(),
        );
    };
    let Some(theme) = object.get("theme") else {
        return ThemePreferenceLoad::Missing;
    };
    let Some(theme) = theme.as_str() else {
        return ThemePreferenceLoad::Ignored(
            "malformed theme preference: theme must be a string".to_owned(),
        );
    };
    if let Some(choice) = ThemeChoice::parse(theme) {
        ThemePreferenceLoad::Loaded(choice.as_str().to_owned())
    } else {
        ThemePreferenceLoad::Ignored("malformed theme preference: unknown theme".to_owned())
    }
}

fn read_preference_object_for_save(path: &Path) -> Result<Map<String, Value>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(anyhow!("could not read preference file: {error}")),
    };
    match serde_json::from_str::<Value>(&contents) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err(anyhow!("malformed preference file: expected object")),
        Err(error) => Err(anyhow!("malformed preference file: {error}")),
    }
}

fn write_preference_object(path: &Path, payload: Map<String, Value>) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("preference path has no parent"))?;
    fs::create_dir_all(parent)?;
    let tmp = temporary_path(path);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)?;
    serde_json::to_writer_pretty(&mut file, &Value::Object(payload))?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    drop(file);
    fs::rename(tmp, path)?;
    Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(PREFERENCE_FILE);
    path.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_and_theme_saves_preserve_each_other() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(".euler").join("preferences.json");

        save_model_preference(&path, "openrouter", "glm-5.2").expect("save");
        save_theme_preference(&path, "light").expect("save theme");
        save_model_preference(&path, "anthropic", "claude-custom").expect("overwrite model");

        let contents = fs::read_to_string(&path).expect("read");
        let value: Value = serde_json::from_str(&contents).expect("json");
        let object = value.as_object().expect("object");
        assert_eq!(
            object.keys().cloned().collect::<Vec<_>>(),
            vec![
                "model".to_owned(),
                "provider".to_owned(),
                "theme".to_owned()
            ]
        );
        assert_eq!(
            load_model_preference(&path),
            PreferenceLoad::Loaded(ModelPreference {
                provider: "anthropic".to_owned(),
                model: "claude-custom".to_owned(),
            })
        );
        assert_eq!(
            load_theme_preference(&path),
            ThemePreferenceLoad::Loaded("gruvbox-light".to_owned())
        );
    }

    #[test]
    fn theme_save_preserves_model_preference() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(".euler").join("preferences.json");

        save_model_preference(&path, "openrouter", "glm-5.2").expect("save model");
        save_theme_preference(&path, "dark").expect("save theme");

        assert_eq!(
            load_model_preference(&path),
            PreferenceLoad::Loaded(ModelPreference {
                provider: "openrouter".to_owned(),
                model: "glm-5.2".to_owned(),
            })
        );
        assert_eq!(
            load_theme_preference(&path),
            ThemePreferenceLoad::Loaded("gruvbox-dark".to_owned())
        );
    }

    #[test]
    fn theme_save_canonicalizes_aliases_and_rejects_unknown_values() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(".euler").join("preferences.json");

        save_theme_preference(&path, " Gruvbox_Light ").expect("save theme");

        let contents = fs::read_to_string(&path).expect("read");
        let value: Value = serde_json::from_str(&contents).expect("json");
        assert_eq!(value["theme"], "gruvbox-light");

        save_theme_preference(&path, "DARK").expect("save theme");

        let contents = fs::read_to_string(&path).expect("read");
        let value: Value = serde_json::from_str(&contents).expect("json");
        assert_eq!(value["theme"], "gruvbox-dark");

        let error = save_theme_preference(&path, "solarized").expect_err("unknown theme");
        assert!(error
            .to_string()
            .contains("theme preference must be one of gruvbox-dark|gruvbox-light|warm-ledger"));
    }

    #[test]
    fn malformed_preference_is_ignored() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("preferences.json");
        fs::write(&path, r#"{"provider":"openrouter"}"#).expect("write");

        assert!(matches!(
            load_model_preference(&path),
            PreferenceLoad::Ignored(message) if message.contains("missing model")
        ));
    }

    #[test]
    fn unreadable_preference_path_is_ignored() {
        let temp = tempfile::tempdir().expect("tempdir");

        assert!(matches!(
            load_model_preference(temp.path()),
            PreferenceLoad::Ignored(message) if message.contains("could not read")
        ));
    }

    #[test]
    fn missing_theme_preference_is_not_malformed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("preferences.json");
        fs::write(&path, r#"{"provider":"openrouter","model":"glm-5.2"}"#).expect("write");

        assert_eq!(load_theme_preference(&path), ThemePreferenceLoad::Missing);
    }

    #[test]
    fn theme_preference_normalizes_known_values_and_ignores_unknown() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("preferences.json");
        fs::write(&path, r#"{"theme":" Light "}"#).expect("write");

        assert_eq!(
            load_theme_preference(&path),
            ThemePreferenceLoad::Loaded("gruvbox-light".to_owned())
        );

        fs::write(&path, r#"{"theme":"dark"}"#).expect("write");

        assert_eq!(
            load_theme_preference(&path),
            ThemePreferenceLoad::Loaded("gruvbox-dark".to_owned())
        );

        fs::write(&path, r#"{"theme":"gruvbox_dark"}"#).expect("write");

        assert_eq!(
            load_theme_preference(&path),
            ThemePreferenceLoad::Loaded("gruvbox-dark".to_owned())
        );

        fs::write(&path, r#"{"theme":"solarized"}"#).expect("write");

        assert!(matches!(
            load_theme_preference(&path),
            ThemePreferenceLoad::Ignored(message) if message.contains("unknown theme")
        ));
    }

    #[test]
    fn saves_do_not_overwrite_malformed_preference_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("preferences.json");
        fs::write(&path, r#"{"provider":"openrouter","#).expect("write");

        let error = save_theme_preference(&path, "light").expect_err("malformed should fail");
        assert!(error.to_string().contains("malformed preference file"));
        assert_eq!(
            fs::read_to_string(&path).expect("read unchanged"),
            r#"{"provider":"openrouter","#
        );

        let error =
            save_model_preference(&path, "openrouter", "glm-5.2").expect_err("malformed fail");
        assert!(error.to_string().contains("malformed preference file"));
        assert_eq!(
            fs::read_to_string(&path).expect("read still unchanged"),
            r#"{"provider":"openrouter","#
        );
    }
}
