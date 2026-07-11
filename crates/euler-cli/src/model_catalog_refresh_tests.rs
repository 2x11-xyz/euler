use super::*;
use euler_provider::catalog::MergedModelCatalog;

#[test]
fn bounded_read_rejects_oversized_response() {
    let error = read_bounded_to_string(std::io::Cursor::new("123456"), 5, "test-source")
        .expect_err("oversized response");

    assert_eq!(
        error.to_string(),
        "failed to read test-source: response exceeds 5 byte limit; models.json left untouched"
    );
}

#[test]
fn translates_tool_call_models_to_overlay_schema() {
    let (overlay, warnings) = translate_modelsdev_json(
        r#"{
          "anthropic":{"models":{"claude-sonnet-5":{"id":"claude-sonnet-5","name":"Claude Sonnet 5","reasoning":true,"tool_call":true,"limit":{"context":1000000,"output":128000}},"claude-text-only":{"id":"claude-text-only","name":"Claude Text","reasoning":false,"tool_call":false,"limit":{"context":200000,"output":8192}}}},
          "openai":{"models":{"gpt-5.5":{"id":"gpt-5.5","name":"GPT-5.5","reasoning":true,"tool_call":true,"limit":{"context":1050000,"output":128000}},"bad":{"id":"bad","name":"Bad","tool_call":true}}},
          "openrouter":{"models":{"z-ai/glm-5.2":{"id":"z-ai/glm-5.2","name":"GLM-5.2","reasoning":true,"tool_call":true,"limit":{"context":1024000,"output":128000}}}},
          "xai":{"models":{"grok-4.3":{"id":"grok-4.3","name":"Grok 4.3","reasoning":true,"tool_call":true,"limit":{"context":1000000,"output":30000}}}}
        }"#,
    )
    .expect("translate");

    assert_eq!(overlay["generated_by"], GENERATED_BY);
    assert_eq!(
        overlay["providers"]["anthropic"]["default_model"],
        DEFAULT_ANTHROPIC_MODEL
    );
    assert_eq!(
        overlay["providers"]["openai"]["models"][0]["context_window_tokens"],
        1_050_000
    );
    assert_eq!(
        overlay["providers"]["chatgpt"]["models"][0]["id"],
        "gpt-5.5"
    );
    assert_eq!(
        overlay["providers"]["openrouter"]["models"][0]["id"],
        "z-ai/glm-5.2"
    );
    assert_eq!(overlay["providers"]["xai"]["models"][0]["id"], "grok-4.3");
    assert_eq!(
        overlay["providers"]["xai"]["default_model"],
        DEFAULT_XAI_MODEL
    );
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("reasoning is missing")));
    assert_eq!(
        overlay["providers"]["anthropic"]["models"]
            .as_array()
            .expect("models")
            .len(),
        1
    );
}

#[test]
fn refresh_overlay_round_trips_through_merged_catalog_without_warnings() {
    let (overlay, warnings) = translate_modelsdev_json(
        r#"{
          "anthropic":{"models":{"claude-sonnet-5":{"id":"claude-sonnet-5","name":"Claude Sonnet 5","reasoning":true,"tool_call":true,"limit":{"context":1000000,"output":128000}}}},
          "openai":{"models":{"gpt-5.5":{"id":"gpt-5.5","name":"GPT-5.5","reasoning":true,"tool_call":true,"limit":{"context":1050000,"output":128000}}}},
          "openrouter":{"models":{"z-ai/glm-5.2":{"id":"z-ai/glm-5.2","name":"GLM-5.2","reasoning":true,"tool_call":true,"limit":{"context":1024000,"output":128000}}}},
          "xai":{"models":{"grok-4.3":{"id":"grok-4.3","name":"Grok 4.3","reasoning":true,"tool_call":true,"limit":{"context":1000000,"output":30000}}}}
        }"#,
    )
    .expect("translate");
    assert!(warnings.is_empty());

    let (catalog, catalog_warnings) =
        MergedModelCatalog::with_local_json(&serde_json::to_string(&overlay).expect("json"));

    assert!(catalog_warnings.is_empty(), "{catalog_warnings:?}");
    assert!(catalog
        .provider("openrouter")
        .expect("openrouter")
        .models()
        .any(|model| model.id() == "z-ai/glm-5.2"));
    assert!(catalog
        .provider("chatgpt")
        .expect("chatgpt")
        .models()
        .any(|model| model.id() == "gpt-5.5"));
    assert!(catalog
        .provider("xai")
        .expect("xai")
        .models()
        .any(|model| model.id() == "grok-4.3"));
}

#[test]
fn generated_catalog_refuses_unmarked_existing_file_without_force() {
    let home = tempfile::tempdir().expect("home");
    let path = home.path().join(".euler").join("models.json");
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(&path, r#"{"version":1,"providers":{}}"#).expect("seed");
    let overlay = json!({"version":1,"generated_by":GENERATED_BY,"providers":{}});

    let error = write_generated_catalog(&path, &overlay, false).expect_err("refuse");
    assert_eq!(error.to_string(), refusal_error(&path));
    assert_eq!(
        fs::read_to_string(&path).expect("read"),
        r#"{"version":1,"providers":{}}"#
    );

    write_generated_catalog(&path, &overlay, true).expect("force write");
    assert_eq!(stored_json(&path)["generated_by"], GENERATED_BY);
}

#[test]
fn generated_catalog_refuses_malformed_or_wrong_marker_without_force() {
    let home = tempfile::tempdir().expect("home");
    let path = home.path().join(".euler").join("models.json");
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let overlay = json!({"version":1,"generated_by":GENERATED_BY,"providers":{}});

    for existing in [
        "not json",
        r#"{"version":1,"generated_by":7,"providers":{}}"#,
        r#"{"version":1,"generated_by":"other tool","providers":{}}"#,
    ] {
        fs::write(&path, existing).expect("seed");
        let error = write_generated_catalog(&path, &overlay, false).expect_err("refuse");
        assert_eq!(error.to_string(), refusal_error(&path));
        assert_eq!(fs::read_to_string(&path).expect("read"), existing);
    }
}

#[test]
fn generated_catalog_uses_deterministic_temp_path() {
    assert_eq!(
        temp_path_for(Path::new("/tmp/example-models.json")),
        Path::new("/tmp/example-models.json.tmp")
    );
}

#[test]
fn generated_catalog_overwrites_own_marked_file() {
    let home = tempfile::tempdir().expect("home");
    let path = home.path().join(".euler").join("models.json");
    let first = json!({"version":1,"generated_by":GENERATED_BY,"providers":{}});
    let second =
        json!({"version":1,"generated_by":GENERATED_BY,"providers":{"openai":{"models":[]}}});

    write_generated_catalog(&path, &first, false).expect("first");
    write_generated_catalog(&path, &second, false).expect("second");

    assert!(stored_json(&path)["providers"].get("openai").is_some());
}

fn refusal_error(path: &Path) -> String {
    format!(
        "{} already exists and was not generated by `euler models refresh`; pass --force to overwrite; models.json left untouched",
        path.display()
    )
}

fn stored_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("read")).expect("stored json")
}
