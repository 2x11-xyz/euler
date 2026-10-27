//! Host validation for `ArgValueKind::ModelTargets` values (ADR 0022).
//!
//! The host checks count and `provider::model` shape on every input route,
//! so a command receives only target strings that passed this check.

use std::collections::BTreeSet;
use thiserror::Error;

/// Host cap on a model-targets argument's declared `max`.
pub const MAX_MODEL_TARGETS: usize = 16;
pub const MAX_MODEL_TARGET_BYTES: usize = 256;

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct ModelTargetsError {
    message: String,
}

impl ModelTargetsError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Accepts 1 to `max` distinct `provider::model` targets.
pub fn validate_model_targets(targets: &[String], max: usize) -> Result<(), ModelTargetsError> {
    if targets.is_empty() || targets.len() > max {
        return Err(ModelTargetsError::new(format!(
            "expected 1 to {max} provider::model targets, got {}",
            targets.len()
        )));
    }
    let mut seen = BTreeSet::new();
    for target in targets {
        validate_model_target(target)?;
        if !seen.insert(target.as_str()) {
            return Err(ModelTargetsError::new(format!(
                "duplicate model target: {target}"
            )));
        }
    }
    Ok(())
}

/// Splits a comma-separated list (`a::b,c::d`) and validates it. Whitespace
/// around each entry is ignored; an empty entry is an error.
pub fn parse_model_targets_list(list: &str, max: usize) -> Result<Vec<String>, ModelTargetsError> {
    let targets = list
        .split(',')
        .map(|target| target.trim().to_owned())
        .collect::<Vec<_>>();
    validate_model_targets(&targets, max)?;
    Ok(targets)
}

fn validate_model_target(target: &str) -> Result<(), ModelTargetsError> {
    if target.is_empty() {
        return Err(ModelTargetsError::new("model target must not be empty"));
    }
    if target.len() > MAX_MODEL_TARGET_BYTES {
        return Err(ModelTargetsError::new(format!(
            "model target exceeds {MAX_MODEL_TARGET_BYTES} bytes"
        )));
    }
    if target
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(ModelTargetsError::new(format!(
            "model target must not contain whitespace: {target}"
        )));
    }
    match target.split_once("::") {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => Ok(()),
        _ => Err(ModelTargetsError::new(format!(
            "model target must use provider::model form: {target}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parse_model_targets_list_accepts_trimmed_targets_within_max() {
        assert_eq!(
            parse_model_targets_list("openai::gpt-5.5, anthropic::claude-opus-4-1", 2),
            Ok(owned(&["openai::gpt-5.5", "anthropic::claude-opus-4-1"]))
        );
    }

    #[test]
    fn parse_model_targets_list_rejects_count_shape_and_duplicates() {
        let cases = [
            ("", "must not be empty"),
            ("openai::a,", "must not be empty"),
            ("openai::a,openai::b,openai::c", "expected 1 to 2"),
            ("gpt-5.5", "provider::model form"),
            ("::gpt-5.5", "provider::model form"),
            ("openai::", "provider::model form"),
            ("openai::a,openai::a", "duplicate model target"),
        ];
        for (list, expected) in cases {
            let error = parse_model_targets_list(list, 2).expect_err(list);
            assert!(
                error.to_string().contains(expected),
                "{list:?}: {error} should mention {expected}"
            );
        }
    }

    #[test]
    fn validate_model_targets_rejects_empty_whitespace_and_oversize_targets() {
        assert!(validate_model_targets(&[], 3)
            .expect_err("empty list")
            .to_string()
            .contains("expected 1 to 3"));
        assert!(validate_model_targets(&owned(&["open ai::x"]), 3)
            .expect_err("whitespace")
            .to_string()
            .contains("whitespace"));
        let oversize = format!("p::{}", "m".repeat(MAX_MODEL_TARGET_BYTES));
        assert!(validate_model_targets(&[oversize], 3)
            .expect_err("oversize")
            .to_string()
            .contains("exceeds"));
    }
}
