#![allow(clippy::too_many_lines)]

use euler_provider::catalog::{MergedModelCatalog, ModelDescriptor, DEFAULT_CHATGPT_MODEL};

fn model<'a>(
    catalog: &'a MergedModelCatalog,
    provider: &str,
    model_id: &str,
) -> &'a ModelDescriptor {
    catalog
        .provider(provider)
        .expect("provider")
        .models()
        .find(|model| model.id() == model_id)
        .expect("model")
}

#[test]
fn local_descriptor_accepts_advisory_metadata() {
    let (catalog, warnings) = MergedModelCatalog::with_local_json(
        r#"{
          "version": 1,
          "providers": {
            "openrouter": {
              "models": [{
                "id": "openai/gpt-metadata",
                "display_name": "GPT Metadata",
                "context_window_tokens": 1048576,
                "max_output_tokens": 65536,
                "supports_tools": true,
                "supports_reasoning": false
              }]
            }
          }
        }"#,
    );

    assert!(warnings.is_empty(), "{warnings:?}");
    let model = model(&catalog, "openrouter", "openai/gpt-metadata");
    assert_eq!(model.context_window_tokens(), Some(1_048_576));
    assert_eq!(model.max_output_tokens(), Some(65_536));
    assert_eq!(model.supports_tools(), Some(true));
    assert_eq!(model.supports_reasoning(), Some(false));
}

#[test]
fn invalid_metadata_fields_warn_but_keep_descriptor() {
    let (catalog, warnings) = MergedModelCatalog::with_local_json(
        r#"{
          "version": 1,
          "providers": {
            "chatgpt": {
              "models": [{
                "id": "gpt-partial",
                "context_window_tokens": "128000",
                "max_output_tokens": 8192,
                "supports_tools": 1,
                "supports_reasoning": true
              }]
            }
          }
        }"#,
    );

    let model = model(&catalog, "chatgpt", "gpt-partial");
    assert_eq!(model.context_window_tokens(), None);
    assert_eq!(model.max_output_tokens(), Some(8192));
    assert_eq!(model.supports_tools(), None);
    assert_eq!(model.supports_reasoning(), Some(true));
    assert!(warnings.iter().any(|message| {
        message.contains("context_window_tokens") && message.contains("positive JSON integer")
    }));
    assert!(warnings
        .iter()
        .any(|message| message.contains("supports_tools") && message.contains("boolean")));
}

#[test]
fn token_metadata_rejects_non_positive_float_null_and_overflow() {
    let (catalog, warnings) = MergedModelCatalog::with_local_json(
        r#"{
          "version": 1,
          "providers": {
            "chatgpt": {
              "models": [
                { "id": "zero", "context_window_tokens": 0 },
                { "id": "negative", "context_window_tokens": -1 },
                { "id": "float", "context_window_tokens": 128000.0 },
                { "id": "nullish", "context_window_tokens": null },
                { "id": "huge", "context_window_tokens": 99999999999999999999 }
              ]
            }
          }
        }"#,
    );

    for id in ["zero", "negative", "float", "nullish", "huge"] {
        assert_eq!(
            model(&catalog, "chatgpt", id).context_window_tokens(),
            None,
            "{id}"
        );
    }
    assert_eq!(
        warnings
            .iter()
            .filter(|message| message.contains("context_window_tokens")
                && message.contains("positive JSON integer"))
            .count(),
        4
    );
    assert_eq!(
        warnings
            .iter()
            .filter(|message| {
                message.contains("context_window_tokens") && message.contains("greater than zero")
            })
            .count(),
        1
    );
}

#[test]
fn built_in_descriptors_include_curated_advisory_metadata() {
    let catalog = MergedModelCatalog::built_in();

    let model = model(&catalog, "chatgpt", DEFAULT_CHATGPT_MODEL);

    assert_eq!(model.context_window_tokens(), Some(1_050_000));
    assert_eq!(model.max_output_tokens(), Some(128_000));
    assert_eq!(model.supports_tools(), Some(true));
    assert_eq!(model.supports_reasoning(), Some(true));
}

#[test]
fn duplicate_descriptors_replace_instead_of_merging_metadata() {
    let (catalog, warnings) = MergedModelCatalog::with_local_json(
        r#"{
          "version": 1,
          "providers": {
            "openai": {
              "models": [
                { "id": "gpt-dup", "supports_tools": true, "context_window_tokens": 128000 },
                { "id": "gpt-dup", "supports_reasoning": true }
              ]
            }
          }
        }"#,
    );

    let model = model(&catalog, "openai", "gpt-dup");
    assert_eq!(model.supports_tools(), None);
    assert_eq!(model.context_window_tokens(), None);
    assert_eq!(model.supports_reasoning(), Some(true));
    assert!(warnings
        .iter()
        .any(|message| message.contains("appeared more than once")));
}

#[test]
fn duplicate_descriptor_with_invalid_metadata_still_replaces_prior_metadata() {
    let (catalog, warnings) = MergedModelCatalog::with_local_json(
        r#"{
          "version": 1,
          "providers": {
            "openai": {
              "models": [
                {
                  "id": "gpt-dup-invalid",
                  "context_window_tokens": 128000,
                  "max_output_tokens": 4096,
                  "supports_tools": true
                },
                {
                  "id": "gpt-dup-invalid",
                  "display_name": "Second Descriptor",
                  "context_window_tokens": "bad",
                  "max_output_tokens": 8192,
                  "supports_reasoning": true
                }
              ]
            }
          }
        }"#,
    );

    let model = model(&catalog, "openai", "gpt-dup-invalid");
    assert_eq!(model.display_name(), "Second Descriptor");
    assert_eq!(model.context_window_tokens(), None);
    assert_eq!(model.max_output_tokens(), Some(8192));
    assert_eq!(model.supports_tools(), None);
    assert_eq!(model.supports_reasoning(), Some(true));
    assert!(warnings
        .iter()
        .any(|message| message.contains("appeared more than once")));
    assert!(warnings.iter().any(|message| {
        message.contains("context_window_tokens") && message.contains("positive JSON integer")
    }));
}

#[test]
fn secret_like_fields_warn_without_values_while_metadata_survives() {
    let (catalog, warnings) = MergedModelCatalog::with_local_json(
        r#"{
          "version": 1,
          "providers": {
            "anthropic": {
              "models": [{
                "id": "claude-secret-field",
                "context_window_tokens": 200000,
                "API_KEY": "SHOULD_NOT_APPEAR"
              }]
            }
          }
        }"#,
    );

    let model = model(&catalog, "anthropic", "claude-secret-field");
    assert_eq!(model.context_window_tokens(), Some(200_000));
    assert!(warnings
        .iter()
        .any(|message| message.contains("forbidden") && message.contains("API_KEY")));
    assert!(!warnings
        .iter()
        .any(|message| message.contains("SHOULD_NOT_APPEAR")));
}
