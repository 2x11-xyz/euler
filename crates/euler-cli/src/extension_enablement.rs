use anyhow::{anyhow, Result};
use euler_core::{EulerHome, ExtensionEnablement, ExtensionRegistry};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const PROJECT_EXTENSION_FILE: &str = ".euler/extensions.json";
const NONE_SELECTION: &str = "none";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ExtensionSelection {
    cli_value: Option<String>,
}

impl ExtensionSelection {
    pub(crate) fn from_cli(value: String) -> Self {
        Self {
            cli_value: Some(value),
        }
    }

    pub(crate) fn is_cli_set(&self) -> bool {
        self.cli_value.is_some()
    }
}

pub(crate) fn resolve_session_extensions(
    root: &Path,
    selection: &ExtensionSelection,
) -> Result<BTreeSet<String>> {
    if let Some(value) = selection.cli_value.as_deref() {
        let valid = valid_registry_ids()?;
        return parse_cli_extensions(value, &valid);
    }
    resolve_registry_project_extensions(root)
}

pub(crate) fn resolve_registry_project_extensions(root: &Path) -> Result<BTreeSet<String>> {
    let mut resolution = RegistryResolution::load()?;
    resolution.apply_project(root)?;
    Ok(resolution.enabled)
}

/// Two-phase resolution so callers can surface registry corruption
/// before doing target-dependent work (the offline runner must fail
/// closed on a corrupt registry even when its session target is bad).
pub(crate) struct RegistryResolution {
    valid: Vec<String>,
    pub(crate) enabled: BTreeSet<String>,
}

impl RegistryResolution {
    pub(crate) fn load() -> Result<Self> {
        let valid = valid_registry_ids()?;
        let registry = ExtensionRegistry::open_read_only(EulerHome::resolve()?);
        let enabled = registry_enabled_set(&registry, &valid)?;
        Ok(Self { valid, enabled })
    }

    pub(crate) fn apply_project(&mut self, root: &Path) -> Result<()> {
        apply_project_overlay(root, &self.valid, &mut self.enabled)
    }
}

fn registry_enabled_set(
    registry: &ExtensionRegistry,
    valid: &[String],
) -> Result<BTreeSet<String>> {
    let valid_set = valid.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let mut enabled = BTreeSet::new();
    for (id, state) in registry.enablement_states()? {
        // Enablement entries for ids no longer present (e.g. formerly bundled
        // extensions) are stale state, not corruption: skip them rather than
        // failing every session after an upgrade.
        if !valid_set.contains(id.as_str()) {
            continue;
        }
        if state == ExtensionEnablement::Enabled {
            enabled.insert(id);
        }
    }
    Ok(enabled)
}

fn parse_cli_extensions(value: &str, valid: &[String]) -> Result<BTreeSet<String>> {
    if value.is_empty() {
        return Err(anyhow!("--extensions requires a value"));
    }
    if value == NONE_SELECTION {
        return Ok(BTreeSet::new());
    }
    let valid_set = valid.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let mut enabled = BTreeSet::new();
    for id in value.split(',') {
        if id.is_empty() {
            return Err(anyhow!("--extensions requires non-empty extension ids"));
        }
        if !valid_set.contains(id) {
            return Err(unknown_cli_id_error(id, valid));
        }
        enabled.insert(id.to_owned());
    }
    Ok(enabled)
}

fn apply_project_overlay(
    root: &Path,
    valid: &[String],
    enabled: &mut BTreeSet<String>,
) -> Result<()> {
    let path = project_extensions_path(root);
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(malformed_project_error(&path, error)),
    };
    let overlay: ProjectExtensions =
        serde_json::from_str(&content).map_err(|error| malformed_project_error(&path, error))?;
    let valid_set = valid.iter().map(String::as_str).collect::<BTreeSet<_>>();
    for id in &overlay.enable {
        if !valid_set.contains(id.as_str()) {
            return Err(unknown_project_id_error(&path, id, valid));
        }
        enabled.insert(id.clone());
    }
    for id in &overlay.disable {
        if !valid_set.contains(id.as_str()) {
            return Err(unknown_project_id_error(&path, id, valid));
        }
        enabled.remove(id);
    }
    Ok(())
}

fn valid_registry_ids() -> Result<Vec<String>> {
    let registry = ExtensionRegistry::open_read_only(EulerHome::resolve()?);
    Ok(registry
        .linked_extensions()?
        .into_iter()
        .map(|linked| linked.id)
        .collect())
}

fn project_extensions_path(root: &Path) -> PathBuf {
    root.join(PROJECT_EXTENSION_FILE)
}

fn unknown_cli_id_error(id: &str, valid: &[String]) -> anyhow::Error {
    anyhow!("unknown extension id: {id}; {}", valid_ids_hint(valid))
}

fn unknown_project_id_error(path: &Path, id: &str, valid: &[String]) -> anyhow::Error {
    anyhow!(
        "unknown extension id in {}: {id}; {}",
        path.display(),
        valid_ids_hint(valid)
    )
}

fn valid_ids_hint(valid: &[String]) -> String {
    if valid.is_empty() {
        "no extensions are linked or installed; add one with `euler extension link` or `euler extension install`".to_owned()
    } else {
        format!("valid ids: {}", valid.join(", "))
    }
}

fn malformed_project_error(path: &Path, error: impl std::fmt::Display) -> anyhow::Error {
    anyhow!(
        "malformed project extensions file {}: {error}",
        path.display()
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectExtensions {
    #[serde(default)]
    enable: Vec<String>,
    #[serde(default)]
    disable: Vec<String>,
}
