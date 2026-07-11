//! CLI-side seam for the persisted code-swarm reviewer config (swarm
//! contract). One resolution chain for every entry point: explicit models on
//! the invocation win outright, then the project tier, then the user tier,
//! then the honest unconfigured failure. Explicit models never mutate the
//! stores.

use euler_core::{resolve_swarm_config, EulerHome, SwarmConfigTier, UNCONFIGURED_SWARM_ERROR};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// User-tier config path, when the euler home resolves.
pub(crate) fn user_config_path() -> Option<PathBuf> {
    EulerHome::resolve()
        .ok()
        .map(|home| home.code_swarm_config_path())
}

/// The workspace root the running process's sessions use (`euler` runs
/// sessions rooted at the launch directory).
pub(crate) fn workspace_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Resolved reviewer targets for display/preselection: persisted tiers only.
/// Errors and unconfigured both yield `None` here; run-time seams surface
/// them honestly through [`apply_config_to_review_input`].
pub(crate) fn resolved_targets_for_display(root: &Path) -> Option<(Vec<String>, SwarmConfigTier)> {
    let (config, tier) = resolve_swarm_config(root, user_config_path().as_deref()).ok()??;
    Some((config.targets(), tier))
}

/// [`apply_config_to_review_input_at`] against the process's real user tier.
pub(crate) fn apply_config_to_review_input(
    root: &Path,
    input: serde_json::Value,
) -> Result<serde_json::Value, String> {
    apply_config_to_review_input_at(root, user_config_path().as_deref(), input)
}

/// Apply the persisted-config steps of the resolution chain to a
/// `code-swarm.review` input object. Explicit `models` in the input pass
/// through untouched (step 1); otherwise persisted config injects `models`
/// and, when the input does not name them, `reviewers` (persona labels) and
/// `max_tokens`. No config anywhere is the honest unconfigured error; a
/// malformed config file is an error, never a silent fall-through.
pub(crate) fn apply_config_to_review_input_at(
    root: &Path,
    user_config_path: Option<&Path>,
    input: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut object = match input {
        serde_json::Value::Null => serde_json::Map::new(),
        serde_json::Value::Object(object) => object,
        other => return Ok(other), // malformed input: let the extension reject it honestly
    };
    if object.get("models").is_some_and(|models| !models.is_null()) {
        return Ok(serde_json::Value::Object(object));
    }
    let resolved = resolve_swarm_config(root, user_config_path)
        .map_err(|error| {
            format!("{error}; fix or delete that file, or rewrite it with /code-swarm")
        })?
        .ok_or_else(|| UNCONFIGURED_SWARM_ERROR.to_owned())?;
    let (config, _tier) = resolved;
    object.insert("models".to_owned(), serde_json::json!(config.targets()));
    if object.get("reviewers").is_none_or(Value::is_null) {
        if let Some(personas) = config.personas() {
            object.insert("reviewers".to_owned(), serde_json::json!(personas));
        }
    }
    if object.get("max_tokens").is_none_or(Value::is_null) {
        if let Some(max_tokens) = config.max_tokens() {
            object.insert("max_tokens".to_owned(), serde_json::json!(max_tokens));
        }
    }
    Ok(serde_json::Value::Object(object))
}

#[cfg(test)]
mod tests {
    use super::*;
    use euler_core::{SwarmConfig, SwarmConfigStore};
    use serde_json::json;

    #[test]
    fn explicit_models_pass_through_untouched() {
        let temp = tempfile::tempdir().expect("temp");
        // Even with a persisted config present, explicit models win.
        SwarmConfigStore::for_project_root(temp.path())
            .save(&SwarmConfig::from_targets(&["a::b"], Some(42)).expect("config"))
            .expect("save");
        let input = json!({"models": ["x::y"], "prompt": "focus"});
        let out = apply_config_to_review_input_at(temp.path(), None, input.clone()).expect("apply");
        assert_eq!(out, input);
    }

    #[test]
    fn persisted_project_config_injects_models_and_max_tokens() {
        let temp = tempfile::tempdir().expect("temp");
        SwarmConfigStore::for_project_root(temp.path())
            .save(&SwarmConfig::from_targets(&["a::b", "c::d"], Some(42)).expect("config"))
            .expect("save");
        let out = apply_config_to_review_input_at(temp.path(), None, json!({"prompt": "focus"}))
            .expect("apply");
        assert_eq!(out["models"], json!(["a::b", "c::d"]));
        assert_eq!(out["max_tokens"], json!(42));
        assert_eq!(out["prompt"], json!("focus"));
    }

    #[test]
    fn unconfigured_yields_the_canonical_remediation_error() {
        let temp = tempfile::tempdir().expect("temp");
        let error = apply_config_to_review_input_at(temp.path(), None, json!({}))
            .expect_err("unconfigured");
        assert_eq!(error, UNCONFIGURED_SWARM_ERROR);
    }

    #[test]
    fn malformed_config_is_an_error_not_a_fallthrough() {
        let temp = tempfile::tempdir().expect("temp");
        let store = SwarmConfigStore::for_project_root(temp.path());
        std::fs::create_dir_all(store.path().parent().expect("dir")).expect("mkdir");
        std::fs::write(store.path(), "{broken").expect("write");
        let error =
            apply_config_to_review_input_at(temp.path(), None, json!({})).expect_err("malformed");
        assert!(
            error.contains("/code-swarm"),
            "remediation present: {error}"
        );
    }
}
