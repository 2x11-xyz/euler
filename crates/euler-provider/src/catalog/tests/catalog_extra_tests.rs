use super::*;

#[test]
fn built_in_catalog_lists_curated_models_with_metadata() {
    let catalog = MergedModelCatalog::built_in();
    let anthropic = catalog.provider("anthropic").expect("anthropic");
    let sonnet = anthropic
        .models()
        .find(|model| model.id() == "claude-sonnet-5")
        .expect("sonnet 5");
    assert_eq!(anthropic.default_model(), "claude-sonnet-5");
    assert_eq!(sonnet.display_name(), "Claude Sonnet 5");
    assert_eq!(sonnet.context_window_tokens(), Some(1_000_000));
    assert_eq!(sonnet.max_output_tokens(), Some(128_000));
    assert_eq!(sonnet.supports_tools(), Some(true));
    assert_eq!(sonnet.supports_reasoning(), Some(true));

    let openai = catalog.provider("openai").expect("openai");
    let gpt55 = openai
        .models()
        .find(|model| model.id() == "gpt-5.5")
        .expect("gpt-5.5");
    assert_eq!(gpt55.context_window_tokens(), Some(1_050_000));
    assert!(openai.models().any(|model| model.id() == "gpt-5.3-codex"));

    let openrouter = catalog.provider("openrouter").expect("openrouter");
    assert!(openrouter
        .models()
        .any(|model| model.id() == "z-ai/glm-5.2"));
    assert!(openrouter
        .models()
        .any(|model| model.id() == "~google/gemini-pro-latest"));
    assert!(openrouter
        .models()
        .any(|model| model.id() == DEFAULT_OPENROUTER_MODEL));
    // The router pseudo-model is only routable as `openrouter/auto`; a bare
    // `auto` id was a PI-import artifact that 404s against the API.
    assert!(openrouter
        .models()
        .any(|model| model.id() == "openrouter/auto"));
    assert!(openrouter.models().all(|model| model.id() != "auto"));
}

#[test]
fn persisted_anthropic_sonnet_4_6_route_survives_default_change() {
    assert_ne!(DEFAULT_ANTHROPIC_MODEL, "claude-sonnet-4-6");
    let ModelSpec::Routed(route) = parse_model_spec("anthropic::claude-sonnet-4-6").expect("route")
    else {
        panic!("persisted model spec should be routed");
    };

    assert_eq!(route.provider().as_str(), ANTHROPIC_PROVIDER_ID);
    assert_eq!(route.model(), "claude-sonnet-4-6");
}

#[test]
fn local_config_rejects_unknown_version_and_malformed_roots() {
    let cases = [
        ("", "models.json is empty"),
        ("[]", "root must be an object"),
        (r#"{"version":2,"providers":{}}"#, "version is not 1"),
        (r#"{"providers":[]}"#, "providers must be an object"),
        ("{", "not valid JSON"),
    ];

    for (json, warning) in cases {
        let (catalog, warnings) = MergedModelCatalog::with_local_json(json);
        assert_eq!(
            catalog.default_model_for_provider("chatgpt"),
            Some(DEFAULT_CHATGPT_MODEL)
        );
        assert!(
            warnings.iter().any(|message| message.contains(warning)),
            "missing warning {warning:?} in {warnings:?}"
        );
    }
}

#[test]
fn local_config_ignores_unknown_noncanonical_and_bad_provider_entries() {
    let (catalog, warnings) = MergedModelCatalog::with_local_json(
        r#"{
          "version": 1,
          "providers": {
            "missing": { "default_model": "ignored" },
            "echo": { "default_model": "ignored-alias" },
            "anthropic": [],
            "chatgpt": { "default_model": "gpt-custom" }
          }
        }"#,
    );

    assert_eq!(
        catalog.default_model_for_provider("fixture"),
        Some(DEFAULT_FIXTURE_MODEL)
    );
    assert_eq!(
        catalog.default_model_for_provider("anthropic"),
        Some(DEFAULT_ANTHROPIC_MODEL)
    );
    assert_eq!(
        catalog.default_model_for_provider("chatgpt"),
        Some("gpt-custom")
    );
    assert!(warnings
        .iter()
        .any(|message| message.contains("unknown provider `missing`")));
    assert!(warnings
        .iter()
        .any(|message| message.contains("non-canonical provider key `echo`")));
    assert!(warnings
        .iter()
        .any(|message| message.contains("not an object")));
}

#[test]
fn local_config_validates_model_ids_and_uses_last_duplicate() {
    let (catalog, warnings) = MergedModelCatalog::with_local_json(
        r#"{
          "version": 1,
          "providers": {
            "chatgpt": {
              "default_model": "bad::route",
              "models": [
                { "id": "" },
                { "id": " spaced " },
                { "id": "bad::route" },
                { "id": "gpt-custom", "display_name": "first" },
                { "id": "gpt-custom", "display_name": "second" }
              ]
            }
          }
        }"#,
    );

    let chatgpt = catalog.provider("chatgpt").expect("chatgpt");
    assert_eq!(chatgpt.default_model(), DEFAULT_CHATGPT_MODEL);
    let model = chatgpt
        .models()
        .find(|model| model.id() == "gpt-custom")
        .expect("duplicate model");
    assert_eq!(model.display_name(), "second");
    assert!(warnings
        .iter()
        .any(|message| message.contains("model id is empty")));
    assert!(warnings
        .iter()
        .any(|message| message.contains("leading or trailing whitespace")));
    assert!(warnings
        .iter()
        .any(|message| message.contains("reserved route separator")));
    assert!(warnings
        .iter()
        .any(|message| message.contains("last valid descriptor wins")));
}

#[test]
fn local_config_warns_for_unknown_and_forbidden_fields_without_values() {
    let (catalog, warnings) = MergedModelCatalog::with_local_json(
        r#"{
          "version": 1,
          "api_key": "SHOULD_NOT_APPEAR",
          "providers": {
            "chatgpt": {
              "headers": { "x-secret": "SHOULD_NOT_APPEAR" },
              "models": [
                {
                  "id": "gpt-custom",
                  "token": "SHOULD_NOT_APPEAR",
                  "extra": "ignored"
                }
              ]
            }
          }
        }"#,
    );

    assert_eq!(
        catalog.default_model_for_provider("chatgpt"),
        Some(DEFAULT_CHATGPT_MODEL)
    );
    let warnings = warnings.join("\n");
    assert!(warnings.contains("forbidden root field `api_key`"));
    assert!(warnings.contains("forbidden provider `chatgpt` field `headers`"));
    assert!(warnings.contains("forbidden provider `chatgpt` model #0 field `token`"));
    assert!(warnings.contains("unknown provider `chatgpt` model #0 field `extra`"));
    assert!(!warnings.contains("SHOULD_NOT_APPEAR"));
}
