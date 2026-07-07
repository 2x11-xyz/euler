use euler_provider::provider_config::ProviderConfigRegistry;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

const PROVIDER_CONFIG_FILE: &str = "providers.json";

pub(crate) struct ProviderConfigLoad {
    pub(crate) registry: ProviderConfigRegistry,
    pub(crate) warnings: Vec<String>,
}

pub(crate) fn default_provider_config_path() -> Option<PathBuf> {
    provider_config_path_from_home_vars(std::env::var_os("HOME"), std::env::var_os("USERPROFILE"))
}

fn provider_config_path_from_home_vars(
    home: Option<OsString>,
    user_profile: Option<OsString>,
) -> Option<PathBuf> {
    home.or(user_profile)
        .map(PathBuf::from)
        .map(|home| home.join(".euler").join(PROVIDER_CONFIG_FILE))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_provider_config_is_silent_builtin_fallback() {
        let temp = tempfile::tempdir().expect("temp dir");
        let load = load_provider_config(Some(&temp.path().join("missing.json")));

        assert!(load.registry.providers.is_empty());
        assert!(load.warnings.is_empty());
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
