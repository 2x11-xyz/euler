use euler_core::{EulerHome, EulerHomeError};
use euler_provider::provider_config::{referenced_env_names, ProviderConfigRegistry};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

const PROVIDER_CONFIG_FILE: &str = "providers.json";

pub(crate) struct ProviderConfigLoad {
    pub(crate) registry: ProviderConfigRegistry,
    pub(crate) warnings: Vec<String>,
}

/// `providers.json` lives in the Euler home, so `EULER_HOME` relocates it with
/// the rest of the user state.
pub(crate) fn resolve_provider_config_path() -> Result<PathBuf, EulerHomeError> {
    provider_config_path_from_env(std::env::var_os("HOME"), std::env::var_os("EULER_HOME"))
}

/// Line-oriented CLI entry: an unresolvable Euler home loads no custom
/// providers and says why on stderr, so a later "unknown provider" error is
/// not the only signal.
pub(crate) fn default_provider_config_path() -> Option<PathBuf> {
    resolve_provider_config_path()
        .map_err(|error| eprintln!("warning: ignored provider config: {error}"))
        .ok()
}

fn provider_config_path_from_env(
    home: Option<OsString>,
    euler_home: Option<OsString>,
) -> Result<PathBuf, EulerHomeError> {
    EulerHome::resolve_from_env(home, euler_home).map(|home| home.root().join(PROVIDER_CONFIG_FILE))
}

pub(crate) fn load_provider_config(path: Option<&Path>) -> ProviderConfigLoad {
    let Some(path) = path else {
        return ProviderConfigLoad {
            registry: ProviderConfigRegistry::default(),
            warnings: Vec::new(),
        };
    };
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ProviderConfigLoad {
                registry: ProviderConfigRegistry::default(),
                warnings: Vec::new(),
            };
        }
        Err(error) => {
            return ProviderConfigLoad {
                registry: ProviderConfigRegistry::default(),
                warnings: vec![format!(
                    "could not read {}: {error}; using built-in providers",
                    path.display()
                )],
            };
        }
    };
    let (registry, warnings) = ProviderConfigRegistry::with_json(&contents);
    ProviderConfigLoad { registry, warnings }
}

/// Environment variables `providers.json` references as `$NAME` or
/// `${NAME}` in an `api_key` or a header value of any provider entry, valid
/// or not ([`referenced_env_names`], the rule the `workspace` sandbox
/// withholds too). A missing or unreadable file names none.
pub(crate) fn referenced_env_variables(path: Option<&Path>) -> BTreeSet<String> {
    path.and_then(|path| fs::read_to_string(path).ok())
        .map(|contents| referenced_env_names(&contents))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn referenced_env_variables_names_every_secret_reference() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("providers.json");
        fs::write(
            &path,
            r#"{"providers": {"proxy": {
                "api_family": "openai_chat_completions",
                "base_url": "https://proxy.example.com/v1",
                "api_key": "$PROXY_API_KEY",
                "headers": {"x-team": "${TEAM_TOKEN}", "x-literal": "$$NOT_A_VAR", "x-bang": "$!literal"},
                "models": [{"id": "m"}]
            }}}"#,
        )
        .expect("write");

        let names = referenced_env_variables(Some(&path));

        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            vec!["PROXY_API_KEY".to_owned(), "TEAM_TOKEN".to_owned()]
        );
        assert!(referenced_env_variables(Some(&temp.path().join("missing.json"))).is_empty());
    }

    #[test]
    fn missing_provider_config_is_silent_builtin_fallback() {
        let temp = tempfile::tempdir().expect("temp dir");
        let load = load_provider_config(Some(&temp.path().join("missing.json")));

        assert!(load.registry.providers.is_empty());
        assert!(load.warnings.is_empty());
    }

    #[test]
    fn provider_config_path_uses_euler_home_when_set() {
        let home = tempfile::tempdir().expect("home dir");
        let euler_home = tempfile::tempdir().expect("euler home dir");

        let path =
            provider_config_path_from_env(Some(home.path().into()), Some(euler_home.path().into()));

        let canonical_euler_home = fs::canonicalize(euler_home.path()).expect("canonical");
        assert_eq!(
            path.expect("resolved path"),
            canonical_euler_home.join("providers.json")
        );
    }

    #[test]
    fn provider_config_path_defaults_to_dot_euler_under_home() {
        let home = tempfile::tempdir().expect("home dir");

        let path = provider_config_path_from_env(Some(home.path().into()), None);

        let canonical_home = fs::canonicalize(home.path()).expect("canonical");
        assert_eq!(
            path.expect("resolved path"),
            canonical_home.join(".euler").join("providers.json")
        );
    }

    #[test]
    fn provider_config_path_reports_relative_euler_home() {
        let home = tempfile::tempdir().expect("home dir");

        let path = provider_config_path_from_env(
            Some(home.path().into()),
            Some("relative-euler-home".into()),
        );

        let error = path.expect_err("relative EULER_HOME must not resolve");
        assert!(error
            .to_string()
            .contains("EULER_HOME must be an absolute path"));
    }

    #[test]
    fn malformed_provider_config_warns_and_falls_back() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("providers.json");
        fs::write(&path, "{").expect("write malformed config");

        let load = load_provider_config(Some(&path));

        assert!(load.registry.providers.is_empty());
        assert_eq!(load.warnings.len(), 1);
        assert!(load.warnings[0].contains("providers.json is not valid JSON"));
    }
}
