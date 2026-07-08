use super::*;
#[test]
fn chatgpt_provider_defaults_to_gpt_55_without_model() {
    let args = parse_without_env(["--provider", "chatgpt"]);

    assert_eq!(args.model, DEFAULT_CHATGPT_MODEL);
}

#[test]
fn explicit_model_overrides_chatgpt_default() {
    let args = parse_without_env(["--provider", "chatgpt", "--model", "custom-model"]);

    assert_eq!(args.model, "custom-model");
}

#[test]
fn openai_provider_defaults_to_openai_default_without_model() {
    let args = parse_without_env(["--provider", "openai"]);

    assert_eq!(args.provider_id, "openai");
    assert_eq!(args.model, DEFAULT_OPENAI_MODEL);
}

#[test]
fn openai_provider_preserves_unknown_explicit_model_ids() {
    let args = parse_without_env(["--provider", "openai", "--model", "future-openai-model"]);

    assert_eq!(args.provider_id, "openai");
    assert_eq!(args.model, "future-openai-model");

    let config = session_config(
        tempfile::tempdir().expect("temp dir").path().to_path_buf(),
        args.provider_id,
        args.model,
        "session-id".to_owned(),
    );
    assert_eq!(config.model, "future-openai-model");
}

#[test]
fn anthropic_provider_defaults_to_sonnet_46_without_model() {
    let args = parse_without_env(["--provider", "anthropic"]);

    assert_eq!(args.model, DEFAULT_ANTHROPIC_MODEL);
}

#[test]
fn openrouter_provider_uses_default_model_without_model_arg() {
    let args = parse_without_env(["--provider", "openrouter"]);

    assert_eq!(args.model, DEFAULT_OPENROUTER_MODEL);
}

#[test]
fn provider_catalog_entries_are_constructible() {
    for descriptor in BUILTIN_PROVIDERS {
        let _ = provider_for_id(
            descriptor.id,
            None,
            &ProviderOptions::default(),
            &ProviderConfigRegistry::default(),
        )
        .expect("construct provider");
    }
}

#[test]
fn provider_release_matrix_defaults_auth_and_factories_are_stable() {
    let expected = [
        ("fixture", DEFAULT_FIXTURE_MODEL, false),
        ("chatgpt", DEFAULT_CHATGPT_MODEL, true),
        ("openai", DEFAULT_OPENAI_MODEL, true),
        ("anthropic", DEFAULT_ANTHROPIC_MODEL, true),
        ("openrouter", DEFAULT_OPENROUTER_MODEL, true),
    ];
    assert_eq!(BUILTIN_PROVIDERS.len(), expected.len());

    for (provider, default_model, auth_file_supported) in expected {
        let descriptor = BUILTIN_PROVIDERS
            .iter()
            .find(|descriptor| descriptor.id == provider)
            .expect("provider descriptor");
        assert_eq!(descriptor.default_model, default_model);
        assert_eq!(descriptor.auth_file_supported, auth_file_supported);

        let args = parse_without_env(["--provider", provider]);
        assert_eq!(args.provider_id, provider);
        assert_eq!(args.model, default_model);
        assert_eq!(args.provider.name(), provider);

        let provider_from_factory = provider_for_id(
            provider,
            None,
            &ProviderOptions::default(),
            &ProviderConfigRegistry::default(),
        )
        .expect("provider factory");
        assert_eq!(provider_from_factory.name(), provider);
    }
}

#[test]
fn routed_model_selects_provider_and_strips_route_prefix() {
    let args = parse_without_env(["--model", "anthropic::claude-custom"]);

    assert_eq!(args.provider_id, "anthropic");
    assert_eq!(args.model, "claude-custom");

    let config = session_config(
        tempfile::tempdir().expect("temp dir").path().to_path_buf(),
        args.provider_id,
        args.model,
        "session-id".to_owned(),
    );
    assert_eq!(config.model, "claude-custom");
    assert!(!config.model.contains("::"));

    let openai = parse_without_env(["--model", "OpenAI::gpt-route"]);
    assert_eq!(openai.provider_id, "openai");
    assert_eq!(openai.model, "gpt-route");
}

#[test]
fn routed_model_splits_once_and_preserves_provider_scoped_model_id() {
    let args = parse_without_env(["--model", "openrouter::huggingface:meta-llama/Llama-2-7b"]);

    assert_eq!(args.provider_id, "openrouter");
    assert_eq!(args.model, "huggingface:meta-llama/Llama-2-7b");

    let config = session_config(
        tempfile::tempdir().expect("temp dir").path().to_path_buf(),
        args.provider_id,
        args.model,
        "session-id".to_owned(),
    );
    assert_eq!(config.provider, "openrouter");
    assert_eq!(config.model, "huggingface:meta-llama/Llama-2-7b");
    assert!(!config.model.starts_with("openrouter::"));
}

#[test]
fn local_model_catalog_does_not_validate_or_rewrite_explicit_route() {
    let temp = tempfile::tempdir().expect("temp dir");
    let catalog_path = temp.path().join("models.json");
    std::fs::write(
        &catalog_path,
        r#"{
          "version": 1,
          "providers": {
            "anthropic": {
              "default_model": "claude-local-default",
              "models": [{ "id": "claude-local-default" }]
            }
          }
        }"#,
    )
    .expect("write catalog");

    let parsed = parse_with_model_catalog(
        ["--model", "anthropic::not-in-local-catalog"],
        EnvArgs::default(),
        &catalog_path,
    );

    assert_eq!(parsed.provider_id, "anthropic");
    assert_eq!(parsed.model, "not-in-local-catalog");
}

#[test]
fn routed_model_normalizes_alias_and_preserves_model_casing() {
    let args = parse_without_env(["--provider", "fixture", "--model", " Echo :: MiXeD::Model "]);

    assert_eq!(args.model, " MiXeD::Model ");

    let alias_provider = parse_without_env(["--provider", "echo", "--model", "fixture::custom"]);
    assert_eq!(alias_provider.provider_id, "fixture");
}

#[test]
fn routed_model_conflicting_provider_fails_before_provider_construction() {
    let error = parse_args_error(["--provider", "openrouter", "--model", "anthropic::claude"]);

    assert_eq!(
        error.to_string(),
        "--provider provider `openrouter` conflicts with route provider `anthropic` in --model"
    );

    let error = parse_args_error(["--provider", "openrouter", "--model", "openai::gpt-4.1"]);
    assert_eq!(
        error.to_string(),
        "--provider provider `openrouter` conflicts with route provider `openai` in --model"
    );
}

#[test]
fn cli_routed_model_overrides_env_provider_when_cli_provider_is_absent() {
    let parsed = parse_run_with_env(
        ["--model", "openrouter::openai/gpt-route"],
        EnvArgs {
            provider: Some("anthropic".to_owned()),
            model: Some("claude-env".to_owned()),
            auth_file: None,
        },
    );

    assert_eq!(parsed.provider_id, "openrouter");
    assert_eq!(parsed.model, "openai/gpt-route");
}

#[test]
fn env_routed_model_selects_provider_or_conflicts_with_env_provider() {
    let selected = parse_run_with_env(
        [],
        EnvArgs {
            provider: None,
            model: Some("anthropic::claude-env-route".to_owned()),
            auth_file: None,
        },
    );
    assert_eq!(selected.provider_id, "anthropic");
    assert_eq!(selected.model, "claude-env-route");

    let error = parse_error_with_env(
        [],
        EnvArgs {
            provider: Some("openrouter".to_owned()),
            model: Some("anthropic::claude-env-route".to_owned()),
            auth_file: None,
        },
    );
    assert_eq!(
        error.to_string(),
        "EULER_PROVIDER provider `openrouter` conflicts with route provider `anthropic` in EULER_MODEL"
    );

    let cli_conflict = parse_error_with_env(
        ["--provider", "openrouter"],
        EnvArgs {
            provider: None,
            model: Some("anthropic::claude-env-route".to_owned()),
            auth_file: None,
        },
    );
    assert_eq!(
        cli_conflict.to_string(),
        "--provider provider `openrouter` conflicts with route provider `anthropic` in EULER_MODEL"
    );
}

#[test]
fn cli_plain_model_wins_over_env_routed_model() {
    let parsed = parse_run_with_env(
        ["--model", "cli-plain-model"],
        EnvArgs {
            provider: Some("anthropic".to_owned()),
            model: Some("openrouter::openai/env-route".to_owned()),
            auth_file: None,
        },
    );

    assert_eq!(parsed.provider_id, "anthropic");
    assert_eq!(parsed.model, "cli-plain-model");
}

#[test]
fn routed_model_rejects_unknown_or_empty_route_provider() {
    assert_eq!(
        parse_args_error(["--provider", "missing"]).to_string(),
        "unknown provider: missing"
    );
    assert_eq!(
        parse_args_error(["--model", "missing::model"]).to_string(),
        "unknown provider: missing"
    );
    assert_eq!(
        parse_args_error(["--model", "::model"]).to_string(),
        "model route provider is empty"
    );
    assert_eq!(
        parse_args_error(["--model", "anthropic::"]).to_string(),
        "model route model is empty"
    );
}

#[test]
fn env_provider_can_select_anthropic_and_model_can_override() {
    let mut args = std::iter::empty();
    let parsed = unwrap_run(
        Args::parse_with_env(
            &mut args,
            EnvArgs {
                provider: Some("anthropic".to_owned()),
                model: Some("claude-custom".to_owned()),
                auth_file: None,
            },
        )
        .expect("args"),
    );

    assert_eq!(parsed.model, "claude-custom");
}

#[test]
fn model_preference_resolution_uses_cli_env_preference_default_order() {
    let temp = tempfile::tempdir().expect("temp dir");
    let preference_path = temp.path().join(".euler").join("preferences.json");
    model_preference::save_model_preference(&preference_path, "openrouter", "persisted-model")
        .expect("save preference");

    let preferred = parse_with_preference([], EnvArgs::default(), &preference_path);
    assert_eq!(preferred.provider_id, "openrouter");
    assert_eq!(preferred.model, "persisted-model");

    let from_env = parse_with_preference(
        [],
        EnvArgs {
            provider: Some("anthropic".to_owned()),
            model: Some("env-model".to_owned()),
            auth_file: None,
        },
        &preference_path,
    );
    assert_eq!(from_env.provider_id, "anthropic");
    assert_eq!(from_env.model, "env-model");

    let env_provider_only = parse_with_preference(
        [],
        EnvArgs {
            provider: Some("anthropic".to_owned()),
            model: None,
            auth_file: None,
        },
        &preference_path,
    );
    assert_eq!(env_provider_only.provider_id, "anthropic");
    assert_eq!(env_provider_only.model, DEFAULT_ANTHROPIC_MODEL);

    let env_provider_matches_preference = parse_with_preference(
        [],
        EnvArgs {
            provider: Some("openrouter".to_owned()),
            model: None,
            auth_file: None,
        },
        &preference_path,
    );
    assert_eq!(env_provider_matches_preference.provider_id, "openrouter");
    assert_eq!(env_provider_matches_preference.model, "persisted-model");

    let from_cli = parse_with_preference(
        ["--provider", "chatgpt", "--model", "cli-model"],
        EnvArgs {
            provider: Some("anthropic".to_owned()),
            model: Some("env-model".to_owned()),
            auth_file: None,
        },
        &preference_path,
    );
    assert_eq!(from_cli.provider_id, "chatgpt");
    assert_eq!(from_cli.model, "cli-model");

    let cli_provider_matches_preference = parse_with_preference(
        ["--provider", "openrouter"],
        EnvArgs::default(),
        &preference_path,
    );
    assert_eq!(cli_provider_matches_preference.provider_id, "openrouter");
    assert_eq!(cli_provider_matches_preference.model, "persisted-model");

    let cli_route_ignores_preference = parse_with_preference(
        ["--model", "anthropic::route-model"],
        EnvArgs::default(),
        &preference_path,
    );
    assert_eq!(cli_route_ignores_preference.provider_id, "anthropic");
    assert_eq!(cli_route_ignores_preference.model, "route-model");

    let cli_provider_only = parse_with_preference(
        ["--provider", "chatgpt"],
        EnvArgs::default(),
        &preference_path,
    );
    assert_eq!(cli_provider_only.provider_id, "chatgpt");
    assert_eq!(cli_provider_only.model, DEFAULT_CHATGPT_MODEL);

    let defaulted = parse_without_env([]);
    assert_eq!(defaulted.provider_id, "fixture");
    assert_eq!(defaulted.model, DEFAULT_FIXTURE_MODEL);
}

#[test]
fn local_model_catalog_default_sits_below_cli_env_and_preference() {
    let temp = tempfile::tempdir().expect("temp dir");
    let catalog_path = temp.path().join(".euler").join("models.json");
    std::fs::create_dir_all(catalog_path.parent().expect("catalog parent")).expect("mkdir");
    std::fs::write(
        &catalog_path,
        r#"{
          "version": 1,
          "providers": {
            "chatgpt": { "default_model": "gpt-local-default" },
            "openai": { "default_model": "gpt-openai-local-default" }
          }
        }"#,
    )
    .expect("write catalog");
    let preference_path = temp.path().join(".euler").join("preferences.json");
    model_preference::save_model_preference(&preference_path, "chatgpt", "gpt-preferred")
        .expect("save preference");

    let local_default =
        parse_with_model_catalog(["--provider", "chatgpt"], EnvArgs::default(), &catalog_path);
    assert_eq!(local_default.provider_id, "chatgpt");
    assert_eq!(local_default.model, "gpt-local-default");

    let openai_default =
        parse_with_model_catalog(["--provider", "openai"], EnvArgs::default(), &catalog_path);
    assert_eq!(openai_default.provider_id, "openai");
    assert_eq!(openai_default.model, "gpt-openai-local-default");

    let from_cli = parse_with_model_catalog(
        ["--provider", "chatgpt", "--model", "gpt-cli"],
        EnvArgs::default(),
        &catalog_path,
    );
    assert_eq!(from_cli.model, "gpt-cli");

    let from_env = parse_with_model_catalog(
        ["--provider", "chatgpt"],
        EnvArgs {
            provider: None,
            model: Some("gpt-env".to_owned()),
            auth_file: None,
        },
        &catalog_path,
    );
    assert_eq!(from_env.model, "gpt-env");

    let from_preference = parse_with_preference_and_catalog(
        ["--provider", "chatgpt"],
        EnvArgs::default(),
        &preference_path,
        &catalog_path,
    );
    assert_eq!(from_preference.model, "gpt-preferred");
}

#[test]
fn local_model_catalog_default_may_reference_model_without_descriptor() {
    let temp = tempfile::tempdir().expect("temp dir");
    let catalog_path = temp.path().join("models.json");
    std::fs::write(
        &catalog_path,
        r#"{
          "version": 1,
          "providers": {
            "openrouter": { "default_model": "brand/new-model" }
          }
        }"#,
    )
    .expect("write catalog");

    let parsed = parse_with_model_catalog(
        ["--provider", "openrouter"],
        EnvArgs::default(),
        &catalog_path,
    );
    assert_eq!(parsed.provider_id, "openrouter");
    assert_eq!(parsed.model, "brand/new-model");

    let config = session_config(
        tempfile::tempdir().expect("temp dir").path().to_path_buf(),
        parsed.provider_id,
        parsed.model,
        "session-id".to_owned(),
    );
    assert_eq!(config.provider, "openrouter");
    assert_eq!(config.model, "brand/new-model");
}

#[test]
fn local_model_catalog_missing_or_malformed_provider_uses_built_in_default() {
    let temp = tempfile::tempdir().expect("temp dir");
    let catalog_path = temp.path().join("models.json");
    std::fs::write(
        &catalog_path,
        r#"{
          "version": 1,
          "providers": {
            "chatgpt": { "default_model": "gpt-local-default" }
          }
        }"#,
    )
    .expect("write catalog");

    let openrouter = parse_with_model_catalog(
        ["--provider", "openrouter"],
        EnvArgs::default(),
        &catalog_path,
    );
    assert_eq!(openrouter.provider_id, "openrouter");
    assert_eq!(openrouter.model, DEFAULT_OPENROUTER_MODEL);

    std::fs::write(&catalog_path, "{").expect("write malformed catalog");
    let chatgpt =
        parse_with_model_catalog(["--provider", "chatgpt"], EnvArgs::default(), &catalog_path);
    assert_eq!(chatgpt.provider_id, "chatgpt");
    assert_eq!(chatgpt.model, DEFAULT_CHATGPT_MODEL);
}

#[test]
fn custom_provider_config_selects_default_and_explicit_models() {
    let temp = tempfile::tempdir().expect("temp dir");
    let providers = write_custom_provider_config(
        temp.path(),
        r#"{
          "version": 1,
          "providers": {
            "local-openai": {
              "api_family": "openai_chat_completions",
              "base_url": "http://localhost:11434/v1",
              "api_key": "$LOCAL_OPENAI_KEY",
              "auth_header": true,
              "default_model": "qwen3-coder",
              "models": [{ "id": "qwen3-coder" }]
            }
          }
        }"#,
    );

    let defaulted = parse_with_provider_config(["--provider", "local-openai"], &providers);
    assert_eq!(defaulted.provider_id, "local-openai");
    assert_eq!(defaulted.model, "qwen3-coder");

    let explicit = parse_with_provider_config(
        ["--provider", "local-openai", "--model", "future-model"],
        &providers,
    );
    assert_eq!(explicit.provider_id, "local-openai");
    assert_eq!(explicit.model, "future-model");
}

#[test]
fn custom_provider_routed_model_splits_once_and_conflicts_cleanly() {
    let temp = tempfile::tempdir().expect("temp dir");
    let providers = write_custom_provider_config(
        temp.path(),
        r#"{
          "providers": {
            "local-openai": {
              "api_family": "openai_chat_completions",
              "base_url": "http://localhost:11434/v1",
              "default_model": "default",
              "models": [{ "id": "default" }]
            }
          }
        }"#,
    );

    let routed =
        parse_with_provider_config(["--model", "LOCAL-OPENAI::model::with::colons"], &providers);
    assert_eq!(routed.provider_id, "local-openai");
    assert_eq!(routed.model, "model::with::colons");

    let matching_provider = parse_with_provider_config(
        [
            "--provider",
            "local-openai",
            "--model",
            "local-openai::custom",
        ],
        &providers,
    );
    assert_eq!(matching_provider.provider_id, "local-openai");
    assert_eq!(matching_provider.model, "custom");

    let conflict = parse_provider_config_error(
        ["--provider", "fixture", "--model", "local-openai::custom"],
        &providers,
    );
    assert_eq!(
        conflict.to_string(),
        "--provider provider `fixture` conflicts with route provider `local-openai` in --model"
    );

    let built_in_conflict = parse_provider_config_error(
        ["--provider", "openai", "--model", "local-openai::custom"],
        &providers,
    );
    assert_eq!(
        built_in_conflict.to_string(),
        "--provider provider `openai` conflicts with route provider `local-openai` in --model"
    );
}

#[test]
fn custom_provider_missing_default_and_auth_file_fail_before_invocation() {
    let temp = tempfile::tempdir().expect("temp dir");
    let providers = write_custom_provider_config(
        temp.path(),
        r#"{
          "providers": {
            "local-openai": {
              "api_family": "openai_chat_completions",
              "base_url": "http://localhost:11434/v1",
              "models": [{ "id": "available" }]
            }
          }
        }"#,
    );

    let missing_default = parse_provider_config_error(["--provider", "local-openai"], &providers);
    assert_eq!(
        missing_default.to_string(),
        "provider `local-openai` has no default_model; pass --model"
    );

    let auth_file = parse_provider_config_error(
        [
            "--provider",
            "local-openai",
            "--model",
            "available",
            "--auth-file",
            "auth.json",
        ],
        &providers,
    );
    assert_eq!(
        auth_file.to_string(),
        "--auth-file is only supported with --provider chatgpt, anthropic, openai, or openrouter"
    );

    let explicit = parse_with_provider_config(
        ["--provider", "local-openai", "--model", "available"],
        &providers,
    );
    assert_eq!(explicit.provider_id, "local-openai");
    assert_eq!(explicit.model, "available");
}

#[test]
fn custom_provider_config_does_not_override_built_in_ids() {
    let temp = tempfile::tempdir().expect("temp dir");
    let providers = write_custom_provider_config(
        temp.path(),
        r#"{
          "providers": {
            "openai": {
              "api_family": "openai_chat_completions",
              "base_url": "http://localhost:11434/v1",
              "default_model": "should-not-win",
              "models": [{ "id": "should-not-win" }]
            }
          }
        }"#,
    );

    let parsed = parse_with_provider_config(["--provider", "openai"], &providers);

    assert_eq!(parsed.provider_id, "openai");
    assert_eq!(parsed.model, DEFAULT_OPENAI_MODEL);
    assert_eq!(parsed.provider.name(), "openai");

    let uppercase = parse_with_provider_config(["--provider", "OpenAI"], &providers);
    assert_eq!(uppercase.provider_id, "openai");
    assert_eq!(uppercase.model, DEFAULT_OPENAI_MODEL);
    assert_eq!(uppercase.provider.name(), "openai");
}

#[test]
fn custom_provider_config_is_available_to_exec_and_resume_provider_sets() {
    let temp = tempfile::tempdir().expect("temp dir");
    let providers = write_custom_provider_config(
        temp.path(),
        r#"{
          "providers": {
            "local-openai": {
              "api_family": "openai_chat_completions",
              "base_url": "http://localhost:11434/v1",
              "default_model": "available",
              "models": [{ "id": "available" }]
            }
          }
        }"#,
    );

    let args = parse_args_with_provider_config(
        ["exec", "--provider", "local-openai", "hello"],
        &providers,
    );
    assert!(matches!(
        args.command,
        Command::Exec(exec) if exec.run.provider_id == "local-openai"
            && exec.run.model == "available"
    ));

    let registry = load_custom_provider_config(Some(&providers));
    let custom_active = resume_provider_set_with_custom(
        &ModelTarget::new("fixture".to_owned(), "echo".to_owned()),
        &ModelTarget::new("local-openai".to_owned(), "available".to_owned()),
        None,
        &registry,
    )
    .expect("custom active provider");
    assert!(custom_active.contains("local-openai"));
    assert!(custom_active.contains("fixture"));

    let custom_original = resume_provider_set_with_custom(
        &ModelTarget::new("local-openai".to_owned(), "available".to_owned()),
        &ModelTarget::new("fixture".to_owned(), "echo".to_owned()),
        None,
        &registry,
    )
    .expect("custom original provider");
    assert!(custom_original.contains("local-openai"));
    assert!(custom_original.contains("fixture"));
}

#[test]
fn models_command_is_top_level_and_rejects_live_run_flags() {
    let args = parse_args_without_env(["models"]);
    assert!(matches!(args.command, Command::Models(ModelsCommand::List)));

    let refresh = parse_args_without_env(["models", "refresh", "--force"]);
    assert!(matches!(
        refresh.command,
        Command::Models(ModelsCommand::Refresh { force: true })
    ));

    for (args, expected) in [
        (
            &["models", "--provider", "chatgpt"][..],
            "--provider is not supported with models",
        ),
        (
            &["models", "--model", "gpt-custom"][..],
            "--model is not supported with models",
        ),
        (
            &["models", "--auth-file", "auth.json"][..],
            "--auth-file is not supported with models",
        ),
        (
            &["models", "--provenance", "events.jsonl"][..],
            "--provenance is not supported with models",
        ),
        (
            &["models", "--no-tty"][..],
            "--no-tty is not supported with models",
        ),
        (
            &["models", "--resume", "session"][..],
            "models cannot be used with --replay or --resume",
        ),
    ] {
        let mut args = args.iter().copied().map(str::to_owned);
        let error = match Args::parse_with_env(&mut args, EnvArgs::default()) {
            Ok(_) => panic!("expected args error"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn interactive_launch_decision_defaults_tui_only_for_implicit_tty() {
    assert_eq!(
        decide_interactive_launch(TuiLaunchIntent {
            default_interactive: true,
            no_tty_arg: false,
            env_no_tty: false,
            stdin_tty: true,
            stdout_tty: true,
        }),
        InteractiveLaunch::Tui
    );
    assert_eq!(
        decide_interactive_launch(TuiLaunchIntent {
            default_interactive: false,
            no_tty_arg: false,
            env_no_tty: false,
            stdin_tty: true,
            stdout_tty: true,
        }),
        InteractiveLaunch::LineOriented
    );
    assert_eq!(
        decide_interactive_launch(TuiLaunchIntent {
            default_interactive: true,
            no_tty_arg: true,
            env_no_tty: false,
            stdin_tty: true,
            stdout_tty: true,
        }),
        InteractiveLaunch::LineOriented
    );
    assert_eq!(
        decide_interactive_launch(TuiLaunchIntent {
            default_interactive: true,
            no_tty_arg: false,
            env_no_tty: true,
            stdin_tty: true,
            stdout_tty: true,
        }),
        InteractiveLaunch::LineOriented
    );
    assert_eq!(
        decide_interactive_launch(TuiLaunchIntent {
            default_interactive: true,
            no_tty_arg: false,
            env_no_tty: false,
            stdin_tty: false,
            stdout_tty: true,
        }),
        InteractiveLaunch::LineOriented
    );
}

#[test]
fn parser_marks_run_no_tty_and_tui_launch_shapes() {
    let implicit = parse_args_without_env([]);
    assert!(implicit.default_interactive);
    assert!(matches!(implicit.command, Command::Run(_)));

    let explicit_run = parse_args_without_env(["run", "--provider", "fixture"]);
    assert!(!explicit_run.default_interactive);
    assert!(matches!(explicit_run.command, Command::Run(_)));

    let no_tty = parse_args_without_env(["--no-tty"]);
    assert!(no_tty.no_tty);
    assert!(matches!(
        no_tty.command,
        Command::Run(run) if !run.linefeed_history_insert
    ));

    let tui = parse_args_without_env(["tui", "--provider", "fixture"]);
    assert!(!tui.default_interactive);
    assert!(matches!(
        tui.command,
        Command::Tui(run) if run.linefeed_history_insert
    ));

    let experimental_tui = parse_args_without_env([
        "tui",
        "--provider",
        "fixture",
        "--experimental-tui-linefeed-history",
    ]);
    assert!(matches!(
        experimental_tui.command,
        Command::Tui(run) if run.linefeed_history_insert
    ));

    let opted_out_tui =
        parse_args_without_env(["tui", "--provider", "fixture", "--no-tui-linefeed-history"]);
    assert!(matches!(
        opted_out_tui.command,
        Command::Tui(run) if !run.linefeed_history_insert
    ));

    let implicit_optout =
        parse_args_without_env(["--provider", "fixture", "--no-tui-linefeed-history"]);
    assert!(implicit_optout.default_interactive);
    assert!(matches!(
        implicit_optout.command,
        Command::Run(run) if !run.linefeed_history_insert && run.linefeed_history_insert_from_cli
    ));

    let mut implicit_run = parse_without_env([]);
    assert!(!implicit_run.linefeed_history_insert);
    assert!(!implicit_run.linefeed_history_insert_from_cli);
    apply_interactive_tui_linefeed_default(&mut implicit_run);
    assert!(implicit_run.linefeed_history_insert);

    let mut implicit_optout_run = parse_without_env(["--no-tui-linefeed-history"]);
    assert!(!implicit_optout_run.linefeed_history_insert);
    assert!(implicit_optout_run.linefeed_history_insert_from_cli);
    apply_interactive_tui_linefeed_default(&mut implicit_optout_run);
    assert!(!implicit_optout_run.linefeed_history_insert);
}

#[test]
fn parser_tracks_default_home_session_vs_explicit_provenance() {
    let implicit = parse_args_without_env([]);
    assert!(!implicit.provenance_from_cli);
    assert_eq!(implicit.live_provenance(), LiveProvenance::HomeSession);

    let explicit = parse_args_without_env(["--provenance", "custom.jsonl"]);
    assert!(explicit.provenance_from_cli);
    assert_eq!(
        explicit.live_provenance(),
        LiveProvenance::Explicit(PathBuf::from("custom.jsonl"))
    );
}

#[test]
fn parser_rejects_tui_no_tty_but_exec_is_unchanged() {
    assert_eq!(
        parse_args_error(["tui", "--no-tty"]).to_string(),
        "tui cannot be combined with --no-tty"
    );
    assert!(matches!(
        parse_args_without_env(["--experimental-tui-linefeed-history"]).command,
        Command::Run(run) if run.linefeed_history_insert && run.linefeed_history_insert_from_cli
    ));
    assert_eq!(
        parse_args_error(["run", "--experimental-tui-linefeed-history"]).to_string(),
        "--experimental-tui-linefeed-history is only supported with tui"
    );
    assert_eq!(
        parse_args_error(["run", "--no-tui-linefeed-history"]).to_string(),
        "--no-tui-linefeed-history is only supported with tui"
    );
    assert_eq!(
        parse_args_error(["--no-tty", "--no-tui-linefeed-history"]).to_string(),
        "--no-tui-linefeed-history is only supported with tui"
    );
    assert_eq!(
        parse_args_error([
            "tui",
            "--experimental-tui-linefeed-history",
            "--no-tui-linefeed-history"
        ])
        .to_string(),
        "--experimental-tui-linefeed-history cannot be combined with --no-tui-linefeed-history"
    );
    assert_eq!(
        parse_args_error([
            "tui",
            "--no-tui-linefeed-history",
            "--experimental-tui-linefeed-history"
        ])
        .to_string(),
        "--no-tui-linefeed-history cannot be combined with --experimental-tui-linefeed-history"
    );
    assert_eq!(
        parse_args_error([
            "tui",
            "--provider",
            "fixture",
            "--experimental-tui-linefeed-history",
            "--experimental-tui-linefeed-history"
        ])
        .to_string(),
        "--experimental-tui-linefeed-history was provided more than once"
    );
    assert_eq!(
        parse_args_error([
            "tui",
            "--provider",
            "fixture",
            "--no-tui-linefeed-history",
            "--no-tui-linefeed-history"
        ])
        .to_string(),
        "--no-tui-linefeed-history was provided more than once"
    );
    assert_eq!(
        parse_args_error([
            "--experimental-tui-linefeed-history",
            "--no-tui-linefeed-history"
        ])
        .to_string(),
        "--experimental-tui-linefeed-history cannot be combined with --no-tui-linefeed-history"
    );
    assert_eq!(
        parse_args_error(["--no-tui-linefeed-history", "--no-tui-linefeed-history"]).to_string(),
        "--no-tui-linefeed-history was provided more than once"
    );
    assert_eq!(
        parse_args_error(["exec", "--experimental-tui-linefeed-history", "run"]).to_string(),
        "--experimental-tui-linefeed-history is only supported with tui"
    );
    assert_eq!(
        parse_args_error(["exec", "--no-tui-linefeed-history", "run"]).to_string(),
        "--no-tui-linefeed-history is only supported with tui"
    );
    assert_eq!(
        parse_args_error(["exec", "run", "--no-tui-linefeed-history"]).to_string(),
        "--no-tui-linefeed-history is only supported with tui"
    );

    let exec = parse_args_without_env(["exec", "run", "checks"]);
    assert!(matches!(
        exec.command,
        Command::Exec(exec) if !exec.run.linefeed_history_insert
    ));
}

#[test]
fn model_preference_malformed_or_unknown_provider_falls_back() {
    let temp = tempfile::tempdir().expect("temp dir");
    let malformed = temp.path().join("malformed.json");
    std::fs::write(&malformed, r#"{"provider":"openrouter"}"#).expect("write malformed");

    let fallback = parse_with_preference([], EnvArgs::default(), &malformed);
    assert_eq!(fallback.provider_id, "fixture");
    assert_eq!(fallback.model, DEFAULT_FIXTURE_MODEL);

    let unknown = temp.path().join("unknown.json");
    model_preference::save_model_preference(&unknown, "missing-provider", "persisted-model")
        .expect("save unknown provider");
    let fallback = parse_with_preference([], EnvArgs::default(), &unknown);
    assert_eq!(fallback.provider_id, "fixture");
    assert_eq!(fallback.model, DEFAULT_FIXTURE_MODEL);
}

#[test]
fn theme_preference_loader_returns_known_theme_only() {
    let temp = tempfile::tempdir().expect("temp dir");
    let preference_path = temp.path().join(".euler").join("preferences.json");

    model_preference::save_theme_preference(&preference_path, "light").expect("save theme");
    assert_eq!(
        load_known_theme_preference(Some(&preference_path)),
        Some(ThemeChoice::GruvboxLight)
    );
    assert_eq!(load_known_theme_preference(None), None);

    std::fs::write(&preference_path, r#"{"theme":"solarized"}"#).expect("write malformed");
    assert_eq!(load_known_theme_preference(Some(&preference_path)), None);
}

#[test]
fn model_preference_does_not_override_resume_target() {
    let temp = tempfile::tempdir().expect("temp dir");
    let preference_path = temp.path().join(".euler").join("preferences.json");
    model_preference::save_model_preference(&preference_path, "openrouter", "persisted-model")
        .expect("save preference");

    let args = parse_args_with_preference(
        ["--resume", "events.jsonl"],
        EnvArgs::default(),
        &preference_path,
    );

    assert!(matches!(
        args.command,
        Command::Resume { path, run }
            if path == std::path::Path::new("events.jsonl")
                && run.provider_id == "fixture"
                && run.model == DEFAULT_FIXTURE_MODEL
    ));
}

#[test]
fn resume_live_target_rejects_conflicting_invocation_provider() {
    let run = parse_without_env(["--provider", "openai"]);
    let error = validate_resume_live_target(
        &run,
        &ModelTarget::new("anthropic".to_owned(), "claude-sonnet".to_owned()),
    )
    .expect_err("provider conflict");

    assert_eq!(
        error.to_string(),
        "resume requires provider anthropic but this invocation configures openai"
    );
}

#[test]
fn resume_provider_set_requires_active_target_but_original_is_best_effort() {
    let providers = resume_provider_set(
        &ModelTarget::new("anthropic".to_owned(), "claude-sonnet".to_owned()),
        &ModelTarget::new("fixture".to_owned(), "echo".to_owned()),
        None,
    )
    .expect("fixture active provider");

    assert!(providers.contains("fixture"));
    assert!(providers.contains("anthropic"));
}

#[test]
fn tui_provider_set_includes_builtin_switch_targets() {
    let run = parse_without_env(["--provider", "fixture"]);

    let providers = tui_provider_set(
        run.provider_id.clone(),
        run.provider,
        &ProviderConfigRegistry::default(),
    );

    assert!(providers.contains("fixture"));
    assert!(providers.contains("chatgpt"));
    assert!(providers.contains("anthropic"));
    assert!(providers.contains("openai"));
    assert!(providers.contains("openrouter"));
}

#[test]
fn tui_provider_set_includes_configured_custom_switch_targets() {
    let temp = tempfile::tempdir().expect("temp dir");
    let sentinel = temp.path().join("secret-command-ran");
    let providers_path = write_custom_provider_config(
        temp.path(),
        &format!(
            r#"{{
          "providers": {{
            "local-openai": {{
              "api_family": "openai_chat_completions",
              "base_url": "http://localhost:11434/v1",
              "api_key": "!touch {}",
              "default_model": "qwen",
              "models": [{{ "id": "qwen" }}]
            }}
          }}
        }}"#,
            sentinel.display()
        ),
    );
    let run = parse_with_provider_config(["--provider", "fixture"], &providers_path);

    let providers = tui_provider_set(run.provider_id.clone(), run.provider, &run.custom_providers);

    assert!(providers.contains("fixture"));
    assert!(providers.contains("local-openai"));
    assert!(!sentinel.exists());
}

#[test]
fn tui_provider_set_allows_cross_provider_switch_model() {
    let run = parse_without_env(["--provider", "fixture"]);
    let provider_id = run.provider_id.clone();
    let model = run.model.clone();
    let providers = tui_provider_set(
        run.provider_id.clone(),
        run.provider,
        &ProviderConfigRegistry::default(),
    );
    let config = session_config(
        PathBuf::from("."),
        provider_id,
        model,
        "tui-switch-test".to_owned(),
    );
    let mut session = Session::new_with_providers(config, providers, CliDecider);

    assert!(session
        .switch_model("openrouter", "openai/gpt-4.1-mini", "user", None)
        .expect("cross-provider switch"));
}

#[test]
fn tui_provider_set_preserves_provider_owned_auth_errors_after_switch() {
    let temp = tempfile::tempdir().expect("temp dir");
    let providers_path = write_custom_provider_config(
        temp.path(),
        r#"{
          "providers": {
            "local-openai": {
              "api_family": "openai_chat_completions",
              "base_url": "http://localhost:11434/v1",
              "auth_header": true,
              "default_model": "qwen",
              "models": [{ "id": "qwen" }]
            }
          }
        }"#,
    );
    let run = parse_with_provider_config(["--provider", "fixture"], &providers_path);
    let provider_id = run.provider_id.clone();
    let model = run.model.clone();
    let providers = tui_provider_set(run.provider_id.clone(), run.provider, &run.custom_providers);
    let config = session_config(
        PathBuf::from("."),
        provider_id,
        model,
        "tui-auth-error-test".to_owned(),
    );
    let mut session = Session::new_with_providers(config, providers, CliDecider);

    assert!(session
        .switch_model("local-openai", "qwen", "user", None)
        .expect("custom switch"));
    let error = session.run_turn("hello").expect_err("missing api key");
    let message = error.to_string();
    assert!(message.contains("custom provider `local-openai` api_key is required"));
    assert!(!message.contains("provider is not configured"));
}

#[test]
fn model_preference_cli_and_env_overrides_do_not_rewrite_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let preference_path = temp.path().join(".euler").join("preferences.json");
    model_preference::save_model_preference(&preference_path, "openrouter", "persisted-model")
        .expect("save preference");
    let before = std::fs::read_to_string(&preference_path).expect("read before");

    let _ = parse_with_preference(
        ["--provider", "chatgpt", "--model", "cli-model"],
        EnvArgs {
            provider: Some("anthropic".to_owned()),
            model: Some("env-model".to_owned()),
            auth_file: None,
        },
        &preference_path,
    );

    assert_eq!(
        std::fs::read_to_string(&preference_path).expect("read after"),
        before
    );
}

#[test]
fn auth_file_is_used_for_anthropic_provider() {
    let args = parse_without_env(["--provider", "anthropic", "--auth-file", "auth.json"]);

    assert_eq!(args.provider_id, "anthropic");
    assert_eq!(args.provider.name(), "anthropic");
    assert_eq!(
        live_options_without_env(["--provider", "anthropic", "--auth-file", "auth.json"]).auth_file,
        Some(PathBuf::from("auth.json"))
    );
}

#[test]
fn env_auth_file_is_used_for_chatgpt_provider() {
    let mut args = ["--provider", "chatgpt"].into_iter().map(str::to_owned);
    let parsed = unwrap_run(
        Args::parse_with_env(
            &mut args,
            EnvArgs {
                provider: None,
                model: None,
                auth_file: Some(PathBuf::from("env-auth.json")),
            },
        )
        .expect("args"),
    );

    assert_eq!(parsed.provider_id, "chatgpt");
    assert_eq!(parsed.provider.name(), "chatgpt");
    assert_eq!(parsed.model, DEFAULT_CHATGPT_MODEL);
    assert_eq!(
        live_options_with_env(
            ["--provider", "chatgpt"],
            EnvArgs {
                provider: None,
                model: None,
                auth_file: Some(PathBuf::from("env-auth.json")),
            },
        )
        .auth_file,
        Some(PathBuf::from("env-auth.json"))
    );
}

#[test]
fn cli_auth_file_is_used_for_chatgpt_provider() {
    let args = parse_without_env(["--provider", "chatgpt", "--auth-file", "cli-auth.json"]);

    assert_eq!(args.provider_id, "chatgpt");
    assert_eq!(args.provider.name(), "chatgpt");
    assert_eq!(args.model, DEFAULT_CHATGPT_MODEL);
    assert_eq!(
        live_options_without_env(["--provider", "chatgpt", "--auth-file", "cli-auth.json"])
            .auth_file,
        Some(PathBuf::from("cli-auth.json"))
    );
}

#[test]
fn explicit_auth_file_uses_codex_shape_and_leaves_file_unchanged() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("codex-auth.json");
    let codex_json = r#"{
  "tokens": {
    "id_token": "id-secret",
    "access_token": "access-secret",
    "refresh_token": "refresh-secret",
    "account_id": "acct-1"
  }
}"#;
    std::fs::write(&path, codex_json).expect("write codex auth");

    let run = parse_without_env([
        "--provider",
        "chatgpt",
        "--auth-file",
        path.to_str().expect("utf8 path"),
    ]);

    run.provider.validate_auth().expect("codex auth shape");
    assert_eq!(std::fs::read_to_string(&path).expect("read"), codex_json);
}

#[test]
fn explicit_auth_file_does_not_parse_euler_auth_storage_shape() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("euler-auth.json");
    std::fs::write(
            &path,
            r#"{"version":1,"providers":{"chatgpt":{"type":"oauth","access":"access-secret","refresh":"refresh-secret","expires":9999999999999,"account_id":"acct-1"}}}"#,
        )
        .expect("write euler auth");

    let run = parse_without_env([
        "--provider",
        "chatgpt",
        "--auth-file",
        path.to_str().expect("utf8 path"),
    ]);
    let error = run
        .provider
        .validate_auth()
        .expect_err("codex parser rejects euler shape");

    assert!(error
        .to_string()
        .contains("failed to parse ChatGPT auth file"));
    assert!(!error.to_string().contains("access-secret"));
    assert!(!error.to_string().contains("refresh-secret"));
}

fn write_api_key_auth_file(path: &std::path::Path, provider: &str, key: &str) {
    let mut storage = euler_core::auth_storage::AuthStorage::new(path).expect("storage");
    storage
        .set(
            provider,
            euler_core::auth_storage::Credential::ApiKey {
                key: euler_core::auth_storage::SecretString::new(key),
            },
        )
        .expect("set api key");
}

#[test]
fn anthropic_cli_auth_file_wires_runtime_provider_auth() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("euler-auth.json");
    write_api_key_auth_file(&path, "anthropic", "anthropic-auth-file-secret");
    let run = parse_without_env([
        "--provider",
        "anthropic",
        "--auth-file",
        path.to_str().expect("utf8 path"),
    ]);

    run.provider.validate_auth().expect("auth file key");
}

#[test]
fn openrouter_cli_auth_file_wires_runtime_provider_auth() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("euler-auth.json");
    write_api_key_auth_file(&path, "openrouter", "openrouter-auth-file-secret");
    let run = parse_without_env([
        "--provider",
        "openrouter",
        "--auth-file",
        path.to_str().expect("utf8 path"),
    ]);

    run.provider.validate_auth().expect("auth file key");
}

#[test]
fn openai_cli_auth_file_wires_runtime_provider_auth() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("euler-auth.json");
    write_api_key_auth_file(&path, "openai", "openai-auth-file-secret");
    let run = parse_without_env([
        "--provider",
        "openai",
        "--auth-file",
        path.to_str().expect("utf8 path"),
    ]);

    run.provider.validate_auth().expect("auth file key");
}

#[test]
fn cli_auth_file_overrides_env_auth_file_for_chatgpt_provider() {
    let mut args = ["--provider", "chatgpt", "--auth-file", "cli-auth.json"]
        .into_iter()
        .map(str::to_owned);
    let parsed = unwrap_run(
        Args::parse_with_env(
            &mut args,
            EnvArgs {
                provider: None,
                model: None,
                auth_file: Some(PathBuf::from("env-auth.json")),
            },
        )
        .expect("args"),
    );

    assert_eq!(parsed.provider_id, "chatgpt");
    assert_eq!(parsed.provider.name(), "chatgpt");
    assert_eq!(
        live_options_with_env(
            ["--provider", "chatgpt", "--auth-file", "cli-auth.json"],
            EnvArgs {
                provider: None,
                model: None,
                auth_file: Some(PathBuf::from("env-auth.json")),
            },
        )
        .auth_file,
        Some(PathBuf::from("cli-auth.json"))
    );
}

#[test]
fn env_auth_file_is_used_for_anthropic_provider() {
    let mut args = ["--provider", "anthropic"].into_iter().map(str::to_owned);
    let parsed = unwrap_run(
        Args::parse_with_env(
            &mut args,
            EnvArgs {
                provider: None,
                model: None,
                auth_file: Some(PathBuf::from("env-auth.json")),
            },
        )
        .expect("args"),
    );

    assert_eq!(parsed.provider_id, "anthropic");
    assert_eq!(parsed.provider.name(), "anthropic");
    assert_eq!(parsed.model, DEFAULT_ANTHROPIC_MODEL);
    assert_eq!(
        live_options_with_env(
            ["--provider", "anthropic"],
            EnvArgs {
                provider: None,
                model: None,
                auth_file: Some(PathBuf::from("env-auth.json")),
            },
        )
        .auth_file,
        Some(PathBuf::from("env-auth.json"))
    );
}

#[test]
fn env_auth_file_is_used_for_openrouter_provider() {
    let mut args = ["--provider", "openrouter"].into_iter().map(str::to_owned);
    let parsed = unwrap_run(
        Args::parse_with_env(
            &mut args,
            EnvArgs {
                provider: None,
                model: None,
                auth_file: Some(PathBuf::from("env-auth.json")),
            },
        )
        .expect("args"),
    );

    assert_eq!(parsed.provider_id, "openrouter");
    assert_eq!(parsed.provider.name(), "openrouter");
    assert_eq!(parsed.model, DEFAULT_OPENROUTER_MODEL);
    assert_eq!(
        live_options_with_env(
            ["--provider", "openrouter"],
            EnvArgs {
                provider: None,
                model: None,
                auth_file: Some(PathBuf::from("env-auth.json")),
            },
        )
        .auth_file,
        Some(PathBuf::from("env-auth.json"))
    );
}

#[test]
fn env_auth_file_is_used_for_openai_provider() {
    let mut args = ["--provider", "openai"].into_iter().map(str::to_owned);
    let parsed = unwrap_run(
        Args::parse_with_env(
            &mut args,
            EnvArgs {
                provider: None,
                model: None,
                auth_file: Some(PathBuf::from("env-auth.json")),
            },
        )
        .expect("args"),
    );

    assert_eq!(parsed.provider_id, "openai");
    assert_eq!(parsed.provider.name(), "openai");
    assert_eq!(parsed.model, DEFAULT_OPENAI_MODEL);
    assert_eq!(
        live_options_with_env(
            ["--provider", "openai"],
            EnvArgs {
                provider: None,
                model: None,
                auth_file: Some(PathBuf::from("env-auth.json")),
            },
        )
        .auth_file,
        Some(PathBuf::from("env-auth.json"))
    );
}

#[test]
fn env_auth_file_is_ignored_for_fixture_provider() {
    let mut args = std::iter::empty();
    let parsed = unwrap_run(
        Args::parse_with_env(
            &mut args,
            EnvArgs {
                provider: None,
                model: None,
                auth_file: Some(PathBuf::from("env-auth.json")),
            },
        )
        .expect("args"),
    );

    assert_eq!(parsed.provider_id, "fixture");
    assert_eq!(parsed.provider.name(), "fixture");
    assert_eq!(parsed.model, DEFAULT_FIXTURE_MODEL);
    assert_eq!(
        live_options_with_env(
            [],
            EnvArgs {
                provider: None,
                model: None,
                auth_file: Some(PathBuf::from("env-auth.json")),
            },
        )
        .auth_file,
        None
    );
}

#[test]
fn cli_auth_file_is_used_for_openrouter_provider() {
    let args = parse_without_env(["--provider", "openrouter", "--auth-file", "auth.json"]);

    assert_eq!(args.provider_id, "openrouter");
    assert_eq!(args.provider.name(), "openrouter");
    assert_eq!(
        live_options_without_env(["--provider", "openrouter", "--auth-file", "auth.json"])
            .auth_file,
        Some(PathBuf::from("auth.json"))
    );
}

#[test]
fn cli_auth_file_is_used_for_openai_provider() {
    let args = parse_without_env(["--provider", "openai", "--auth-file", "auth.json"]);

    assert_eq!(args.provider_id, "openai");
    assert_eq!(args.provider.name(), "openai");
    assert_eq!(
        live_options_without_env(["--provider", "openai", "--auth-file", "auth.json"]).auth_file,
        Some(PathBuf::from("auth.json"))
    );
}

#[test]
fn cli_auth_file_is_rejected_for_fixture_provider() {
    let mut args = ["--auth-file", "auth.json"].into_iter().map(str::to_owned);

    let error = match Args::parse_with_env(&mut args, EnvArgs::default()) {
        Ok(_) => panic!("expected args error"),
        Err(error) => error,
    };

    assert_eq!(
        error.to_string(),
        "--auth-file is only supported with --provider chatgpt, anthropic, openai, or openrouter"
    );
}

#[test]
fn fixture_provider_keeps_echo_default() {
    let args = parse_without_env([]);

    assert_eq!(args.provider_id, "fixture");
    assert_eq!(args.provider.name(), "fixture");
    assert_eq!(args.model, DEFAULT_FIXTURE_MODEL);
}

#[test]
fn provider_option_parser_splits_on_first_equals_and_rejects_duplicates() {
    let mut options = ProviderOptions::default();
    options
        .insert("event-script=/tmp/a=b.json")
        .expect("insert");

    assert_eq!(
        options.values.get("event-script").map(String::as_str),
        Some("/tmp/a=b.json")
    );
    assert_eq!(
        options
            .insert("event-script=/tmp/other.json")
            .expect_err("duplicate")
            .to_string(),
        "duplicate provider option: event-script"
    );
}

#[test]
fn provider_option_parser_rejects_malformed_keys() {
    let cases = [
        ("missing-equals", "--provider-option requires key=value"),
        ("=value", "--provider-option key cannot be empty"),
        (
            "event script=value",
            "invalid provider option key: event script",
        ),
        (
            "event-script =value",
            "--provider-option does not allow whitespace around key or value",
        ),
        (
            "event-script= value",
            "--provider-option does not allow whitespace around key or value",
        ),
    ];

    for (option, expected) in cases {
        let mut options = ProviderOptions::default();
        let error = options.insert(option).expect_err("malformed option");

        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn provider_options_are_fresh_session_only_for_first_slice() {
    let cases = [
        (
            [
                "--replay",
                "events.jsonl",
                "--provider-option",
                "event-script=a.json",
            ],
            "--provider-option is not supported with --replay",
        ),
        (
            [
                "--resume",
                "events.jsonl",
                "--provider-option",
                "event-script=a.json",
            ],
            "--provider-option is not supported with --resume",
        ),
        (
            [
                "--provider",
                "chatgpt",
                "--provider-option",
                "event-script=a.json",
            ],
            "provider option `event-script` is not supported by provider chatgpt",
        ),
        (
            [
                "--provider",
                "fixture",
                "--provider-option",
                "event-script=",
            ],
            "provider option `event-script` requires a value",
        ),
        (
            ["--provider", "fixture", "--provider-option", "other=value"],
            "provider option `other` is not supported by provider fixture",
        ),
    ];

    for (args, expected) in cases {
        assert_eq!(parse_args_error(args).to_string(), expected);
    }

    assert_eq!(
        parse_args_error([
            "exec",
            "--replay",
            "events.jsonl",
            "--provider-option",
            "event-script=a.json",
            "prompt",
        ])
        .to_string(),
        "exec cannot be used with --replay"
    );
}

#[test]
fn observe_flag_parses_valid_extension_and_default_cadence() {
    let args = parse_without_env(["--observe", "causal-dag"]);

    assert_eq!(args.observe.extension_id.as_deref(), Some("causal-dag"));
    assert_eq!(args.observe.cadence_rounds.map(NonZeroU64::get), Some(8));
}

#[test]
fn observe_flag_rejects_unknown_extension() {
    let error = parse_args_error(["--observe", "not-bundled"]);

    // Only observer-capable extensions are suggested, not every bundled id.
    assert_eq!(
        error.to_string(),
        "unknown extension id for --observe: not-bundled; observer-capable extensions: causal-dag"
    );
}

#[test]
fn observe_flag_rejects_observer_incapable_extension() {
    let error = parse_args_error(["--observe", "session-export"]);

    assert_eq!(
        error.to_string(),
        "--observe session-export is not supported: extension session-export declares no observer command pair"
    );
}

#[test]
fn observe_flag_parse_edge_cases() {
    assert_eq!(
        parse_args_error(["--observe", "causal-dag", "--observe", "causal-dag"]).to_string(),
        "--observe was provided more than once"
    );
    assert_eq!(
        parse_args_error([
            "--observe",
            "causal-dag",
            "--observe-cadence",
            "4",
            "--observe-cadence",
            "4",
        ])
        .to_string(),
        "--observe-cadence was provided more than once"
    );
    assert_eq!(
        parse_args_error(["--observe", "causal-dag", "--observe-cadence", "0"]).to_string(),
        "--observe-cadence requires a positive integer"
    );
    assert_eq!(
        parse_args_error(["--observe-cadence"]).to_string(),
        "--observe-cadence requires a value"
    );
}

#[test]
fn observe_cadence_requires_observe() {
    assert_eq!(
        parse_args_error(["--observe-cadence", "4"]).to_string(),
        "--observe-cadence requires --observe"
    );
}

#[test]
fn observe_requires_extension_enabled_set() {
    let run = parse_without_env(["--observe", "causal-dag", "--extensions", "none"]);
    let root = tempfile::tempdir().expect("root");
    let enabled = resolve_session_extensions(root.path(), &run.extensions).expect("extensions");

    let error = match bundled_round_observer(&run.observe, &enabled) {
        Ok(_) => panic!("expected observer disabled"),
        Err(error) => error,
    };

    assert_eq!(
        error.to_string(),
        "--observe causal-dag requires extension causal-dag to be enabled; enable it with --extensions causal-dag or your Euler extension registry/project config"
    );
}

#[test]
fn observer_wiring_uses_bundled_command_pair() {
    let run = parse_without_env(["--observe", "causal-dag", "--extensions", "causal-dag"]);
    let root = tempfile::tempdir().expect("root");
    let enabled = resolve_session_extensions(root.path(), &run.extensions).expect("extensions");
    let (observer, _) = bundled_round_observer(&run.observe, &enabled)
        .expect("observer")
        .expect("configured");
    let descriptor = bundled_descriptor_by_id("causal-dag")
        .expect("descriptor")
        .expect("causal-dag descriptor");
    let commands = descriptor
        .observer_commands
        .expect("causal-dag observer commands");
    let mut config = session_config(
        root.path().to_path_buf(),
        run.provider_id,
        run.model,
        "session-id".to_owned(),
    );
    config.round_observer = Some(observer);
    let observer = config.round_observer.expect("observer config");

    assert_eq!(observer.brief_command, commands.brief);
    assert_eq!(observer.apply_command, commands.apply);
}

#[test]
fn echo_provider_alias_uses_fixture_provider_id() {
    let args = parse_without_env(["--provider", "echo"]);

    assert_eq!(args.provider_id, "fixture");
    assert_eq!(args.provider.name(), "fixture");
    assert_eq!(args.model, DEFAULT_FIXTURE_MODEL);
}

#[test]
fn env_echo_provider_alias_uses_fixture_provider_id() {
    let mut args = std::iter::empty();
    let parsed = unwrap_run(
        Args::parse_with_env(
            &mut args,
            EnvArgs {
                provider: Some("echo".to_owned()),
                model: None,
                auth_file: None,
            },
        )
        .expect("args"),
    );

    assert_eq!(parsed.provider_id, "fixture");
    assert_eq!(parsed.provider.name(), "fixture");
    assert_eq!(parsed.model, DEFAULT_FIXTURE_MODEL);
}

#[test]
fn replay_skips_live_provider_validation() {
    let args = parse_args_without_env([
        "--replay",
        "events.jsonl",
        "--provider",
        "missing-provider",
        "--auth-file",
        "irrelevant-auth.json",
    ]);

    assert!(matches!(
        args.command,
        Command::Replay { path } if path == std::path::Path::new("events.jsonl")
    ));
}

#[test]
fn resume_parses_path_with_live_provider_defaults() {
    let args = parse_args_without_env(["--resume", "events.jsonl", "--provider", "chatgpt"]);

    assert!(matches!(
        args.command,
        Command::Resume { path, run }
            if path == std::path::Path::new("events.jsonl")
                && run.provider_id == "chatgpt"
                && run.model == DEFAULT_CHATGPT_MODEL
    ));
}

#[test]
fn login_parse_accepts_chatgpt_provider() {
    let args = parse_args_without_env(["login", "--provider", "chatgpt"]);

    assert!(matches!(
        args.command,
        Command::Login(LoginArgs { provider_id }) if provider_id == "chatgpt"
    ));
}

#[test]
fn logout_parse_accepts_chatgpt_provider() {
    let args = parse_args_without_env(["logout", "--provider", "chatgpt"]);

    assert!(matches!(
        args.command,
        Command::Logout(LogoutArgs { provider_id }) if provider_id == "chatgpt"
    ));
}

#[test]
fn auth_status_parse_accepts_minimal_command() {
    let args = parse_args_without_env(["auth", "status"]);

    assert!(matches!(args.command, Command::AuthStatus));
}

#[test]
fn login_parse_rejects_missing_cli_provider_even_when_env_selects_chatgpt() {
    let mut args = ["login"].into_iter().map(str::to_owned);
    let error = match Args::parse_with_env(
        &mut args,
        EnvArgs {
            provider: Some("chatgpt".to_owned()),
            model: None,
            auth_file: None,
        },
    ) {
        Ok(_) => panic!("expected login error"),
        Err(error) => error,
    };

    assert_eq!(
        error.to_string(),
        "login requires explicit --provider chatgpt"
    );
}

#[test]
fn login_and_logout_require_explicit_cli_provider() {
    let login_error = parse_args_error(["login"]);
    let logout_error = parse_args_error(["logout"]);

    assert_eq!(
        login_error.to_string(),
        "login requires explicit --provider chatgpt"
    );
    assert_eq!(
        logout_error.to_string(),
        "logout requires explicit --provider chatgpt"
    );
}

#[test]
fn login_parse_rejects_invalid_shapes() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["--provider", "chatgpt", "login"],
            "login must be the first argument",
        ),
        (
            &["login", "--provider", "anthropic"],
            "login is only supported with --provider chatgpt",
        ),
        (
            &["login", "--provider", "chatgpt", "--auth-file", "auth.json"],
            "--auth-file is not supported with login",
        ),
        (
            &["login", "--provider", "chatgpt", "--resume", "events.jsonl"],
            "login cannot be used with --replay or --resume",
        ),
        (
            &["login", "--provider", "chatgpt", "--replay", "events.jsonl"],
            "login cannot be used with --replay or --resume",
        ),
        (
            &["login", "--provider", "chatgpt", "login"],
            "login command was provided more than once",
        ),
        (
            &["login", "--provider", "chatgpt", "--model", "gpt-test"],
            "--model is not supported with login",
        ),
        (
            &[
                "login",
                "--provider",
                "chatgpt",
                "--provenance",
                "events.jsonl",
            ],
            "--provenance is not supported with login",
        ),
    ];

    for (args, expected) in cases {
        let mut args = args.iter().copied().map(str::to_owned);
        let error = match Args::parse_with_env(&mut args, EnvArgs::default()) {
            Ok(_) => panic!("expected login error"),
            Err(error) => error,
        };

        assert_eq!(error.to_string(), *expected);
    }
}

#[test]
fn logout_parse_rejects_invalid_shapes() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["--provider", "chatgpt", "logout"],
            "logout must be the first argument",
        ),
        (
            &["logout", "--provider", "anthropic"],
            "logout is only supported with --provider chatgpt",
        ),
        (&["logout"], "logout requires explicit --provider chatgpt"),
        (
            &[
                "logout",
                "--provider",
                "chatgpt",
                "--auth-file",
                "auth.json",
            ],
            "--auth-file is not supported with logout",
        ),
        (
            &[
                "logout",
                "--provider",
                "chatgpt",
                "--resume",
                "events.jsonl",
            ],
            "logout cannot be used with --replay or --resume",
        ),
        (
            &[
                "logout",
                "--provider",
                "chatgpt",
                "--replay",
                "events.jsonl",
            ],
            "logout cannot be used with --replay or --resume",
        ),
        (
            &["logout", "--provider", "chatgpt", "--model", "gpt-test"],
            "--model is not supported with logout",
        ),
        (
            &[
                "logout",
                "--provider",
                "chatgpt",
                "--provenance",
                "events.jsonl",
            ],
            "--provenance is not supported with logout",
        ),
    ];

    for (args, expected) in cases {
        let mut args = args.iter().copied().map(str::to_owned);
        let error = match Args::parse_with_env(&mut args, EnvArgs::default()) {
            Ok(_) => panic!("expected logout error"),
            Err(error) => error,
        };

        assert_eq!(error.to_string(), *expected);
    }
}

#[test]
fn auth_status_parse_rejects_invalid_shapes() {
    let cases: &[(&[&str], &str)] = &[
        (&["auth"], "auth requires a subcommand"),
        (&["auth", "login"], "unknown auth subcommand: login"),
        (
            &["--provider", "chatgpt", "auth", "status"],
            "auth must be the first argument",
        ),
        (
            &["auth", "status", "--provider", "chatgpt"],
            "--provider is not supported with auth status",
        ),
        (
            &["auth", "status", "--auth-file", "auth.json"],
            "--auth-file is not supported with auth status",
        ),
        (
            &["auth", "status", "--resume", "events.jsonl"],
            "auth status cannot be used with --replay or --resume",
        ),
        (
            &["auth", "status", "--replay", "events.jsonl"],
            "auth status cannot be used with --replay or --resume",
        ),
    ];

    for (args, expected) in cases {
        let mut args = args.iter().copied().map(str::to_owned);
        let error = match Args::parse_with_env(&mut args, EnvArgs::default()) {
            Ok(_) => panic!("expected auth status error"),
            Err(error) => error,
        };

        assert_eq!(error.to_string(), *expected);
    }
}

#[test]
fn top_level_commands_are_mutually_exclusive() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["login", "auth", "status"],
            "login cannot be combined with auth status",
        ),
        (&["logout", "login"], "logout cannot be combined with login"),
        (
            &["auth", "status", "logout"],
            "auth status cannot be combined with logout",
        ),
        (
            &["auth", "status", "login"],
            "auth status cannot be combined with login",
        ),
    ];

    for (args, expected) in cases {
        let mut args = args.iter().copied().map(str::to_owned);
        let error = match Args::parse_with_env(&mut args, EnvArgs::default()) {
            Ok(_) => panic!("expected mixed-command error"),
            Err(error) => error,
        };

        assert_eq!(error.to_string(), *expected);
    }
}

fn parse_without_env<const N: usize>(args: [&str; N]) -> RunArgs {
    unwrap_run(parse_args_without_env(args))
}

fn parse_run_with_env<const N: usize>(args: [&str; N], env: EnvArgs) -> RunArgs {
    let mut args = args.into_iter().map(str::to_owned);
    unwrap_run(Args::parse_with_env(&mut args, env).expect("args"))
}

fn parse_args_without_env<const N: usize>(args: [&str; N]) -> Args {
    parse_args_without_env_result(args).expect("args")
}

fn parse_args_without_env_result<const N: usize>(args: [&str; N]) -> Result<Args> {
    let mut args = args.into_iter().map(str::to_owned);
    Args::parse_with_env(&mut args, EnvArgs::default())
}

fn parse_args_error<const N: usize>(args: [&str; N]) -> anyhow::Error {
    match parse_args_without_env_result(args) {
        Ok(_) => panic!("expected args error"),
        Err(error) => error,
    }
}

fn parse_error_with_env<const N: usize>(args: [&str; N], env: EnvArgs) -> anyhow::Error {
    let mut args = args.into_iter().map(str::to_owned);
    match Args::parse_with_env(&mut args, env) {
        Ok(_) => panic!("expected args error"),
        Err(error) => error,
    }
}

fn unwrap_run(args: Args) -> RunArgs {
    match args.command {
        Command::Run(run) => run,
        Command::Tui(_) => panic!("expected run args"),
        Command::Replay { .. } => panic!("expected run args"),
        Command::Exec(_) => panic!("expected run args"),
        Command::Resume { .. } => panic!("expected run args"),
        Command::Login(_) => panic!("expected run args"),
        Command::Logout(_) => panic!("expected run args"),
        Command::AuthStatus => panic!("expected run args"),
        Command::Models(_) => panic!("expected run args"),
        Command::SessionExport(_) => panic!("expected run args"),
        Command::Extension(_) => panic!("expected run args"),
    }
}

fn live_options_without_env<const N: usize>(args: [&str; N]) -> LiveOptions {
    live_options_with_env(args, EnvArgs::default())
}

fn live_options_with_env<const N: usize>(args: [&str; N], env: EnvArgs) -> LiveOptions {
    let mut args = args.into_iter().map(str::to_owned);
    let raw = RawArgs::parse_with_env(&mut args, env).expect("raw args");
    resolve_live_options(&raw, None, &ProviderConfigRegistry::default()).expect("live options")
}

fn parse_with_preference<const N: usize>(
    args: [&str; N],
    env: EnvArgs,
    preference_path: &std::path::Path,
) -> RunArgs {
    unwrap_run(parse_args_with_preference(args, env, preference_path))
}

fn parse_args_with_preference<const N: usize>(
    args: [&str; N],
    env: EnvArgs,
    preference_path: &std::path::Path,
) -> Args {
    let mut args = args.into_iter().map(str::to_owned);
    Args::parse_with_env_and_preference_path(&mut args, env, preference_path).expect("args")
}

fn parse_with_model_catalog<const N: usize>(
    args: [&str; N],
    env: EnvArgs,
    model_catalog_path: &std::path::Path,
) -> RunArgs {
    unwrap_run(parse_args_with_model_catalog(args, env, model_catalog_path))
}

fn parse_args_with_model_catalog<const N: usize>(
    args: [&str; N],
    env: EnvArgs,
    model_catalog_path: &std::path::Path,
) -> Args {
    let mut args = args.into_iter().map(str::to_owned);
    Args::parse_with_env_preference_and_catalog_path(&mut args, env, None, model_catalog_path)
        .expect("args")
}

fn parse_with_provider_config<const N: usize>(
    args: [&str; N],
    provider_config_path: &std::path::Path,
) -> RunArgs {
    unwrap_run(parse_args_with_provider_config(args, provider_config_path))
}

fn parse_args_with_provider_config<const N: usize>(
    args: [&str; N],
    provider_config_path: &std::path::Path,
) -> Args {
    let mut args = args.into_iter().map(str::to_owned);
    Args::parse_with_env_preference_catalog_and_provider_config(
        &mut args,
        EnvArgs::default(),
        None,
        None,
        Some(provider_config_path),
    )
    .expect("args")
}

fn parse_provider_config_error<const N: usize>(
    args: [&str; N],
    provider_config_path: &std::path::Path,
) -> anyhow::Error {
    let mut args = args.into_iter().map(str::to_owned);
    match Args::parse_with_env_preference_catalog_and_provider_config(
        &mut args,
        EnvArgs::default(),
        None,
        None,
        Some(provider_config_path),
    ) {
        Ok(_) => panic!("expected args error"),
        Err(error) => error,
    }
}

fn write_custom_provider_config(root: &std::path::Path, contents: &str) -> std::path::PathBuf {
    let path = root.join("providers.json");
    std::fs::write(&path, contents).expect("write providers config");
    path
}

fn parse_with_preference_and_catalog<const N: usize>(
    args: [&str; N],
    env: EnvArgs,
    preference_path: &std::path::Path,
    model_catalog_path: &std::path::Path,
) -> RunArgs {
    let mut args = args.into_iter().map(str::to_owned);
    unwrap_run(
        Args::parse_with_env_preference_and_catalog_path(
            &mut args,
            env,
            Some(preference_path),
            model_catalog_path,
        )
        .expect("args"),
    )
}

#[test]
fn extensions_flag_is_rejected_where_no_session_exists() {
    for (args, expected) in [
        (
            &["models", "--extensions", "maxproof"][..],
            "--extensions is not supported with models",
        ),
        (
            &["login", "--provider", "chatgpt", "--extensions", "maxproof"][..],
            "--extensions is not supported with login",
        ),
        (
            &[
                "logout",
                "--provider",
                "chatgpt",
                "--extensions",
                "maxproof",
            ][..],
            "--extensions is not supported with logout",
        ),
        (
            &["auth", "status", "--extensions", "maxproof"][..],
            "--extensions is not supported with auth status",
        ),
        (
            &["session-export", "events.jsonl", "--extensions", "maxproof"][..],
            "--extensions is not supported with session-export",
        ),
        (
            &["session-export", "--extensions", "maxproof"][..],
            "session-export requires a session id, name, or events path before `--extensions`",
        ),
        (
            &["--replay", "events.jsonl", "--extensions", "maxproof"][..],
            "--extensions is not supported with --replay",
        ),
        (
            &["extension", "list", "--extensions", "maxproof"][..],
            "extension list does not accept arguments: --extensions",
        ),
    ] {
        let mut args = args.iter().copied().map(str::to_owned);
        let error = match Args::parse_with_env(&mut args, EnvArgs::default()) {
            Ok(_) => panic!("expected args error for {args:?}"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn exec_reasoning_effort_parses_and_reaches_run_args() {
    let mut args = ["exec", "--reasoning-effort", "xlarge", "hello"]
        .iter()
        .map(|s| (*s).to_owned());
    let parsed = Args::parse_with_env(&mut args, EnvArgs::default()).expect("parse");
    match parsed.command {
        Command::Exec(exec) => {
            assert_eq!(exec.run.reasoning_effort, Some(ReasoningEffort::XLarge));
        }
        _ => panic!("expected exec command"),
    }
}

#[test]
fn exec_max_tool_rounds_parses_and_reaches_run_args() {
    let mut args = ["exec", "--max-tool-rounds", "300", "hello"]
        .iter()
        .map(|s| (*s).to_owned());
    let parsed = Args::parse_with_env(&mut args, EnvArgs::default()).expect("parse");
    match parsed.command {
        Command::Exec(exec) => {
            assert_eq!(exec.run.max_tool_rounds, Some(300));
        }
        _ => panic!("expected exec command"),
    }
}

#[test]
fn exec_auto_compaction_flags_parse_and_reach_run_args() {
    let mut args = [
        "exec",
        "--auto-compaction",
        "off",
        "--compaction-budget-bytes",
        "4096",
        "hello",
    ]
    .iter()
    .map(|s| (*s).to_owned());
    let parsed = Args::parse_with_env(&mut args, EnvArgs::default()).expect("parse");
    match parsed.command {
        Command::Exec(exec) => {
            assert_eq!(exec.run.auto_compaction, Some(CompactionTier::Off));
            assert_eq!(exec.run.compaction_budget_bytes, Some(4096));
        }
        _ => panic!("expected exec command"),
    }
}

#[test]
fn auto_compaction_flags_rejected_outside_exec_and_bad_values() {
    for (args, expected) in [
        (
            &["run", "--auto-compaction", "stubs"][..],
            "--auto-compaction is only supported with exec",
        ),
        (
            &["exec", "--auto-compaction", "assisted", "hi"][..],
            "--auto-compaction must be one of off|stubs",
        ),
        (
            &[
                "exec",
                "--auto-compaction",
                "off",
                "--auto-compaction",
                "stubs",
                "hi",
            ][..],
            "--auto-compaction was provided more than once",
        ),
        (
            &["run", "--compaction-budget-bytes", "4096"][..],
            "--compaction-budget-bytes is only supported with exec",
        ),
        (
            &["exec", "--compaction-budget-bytes", "0", "hi"][..],
            "--compaction-budget-bytes requires a positive integer",
        ),
        (
            &[
                "exec",
                "--compaction-budget-bytes",
                "1",
                "--compaction-budget-bytes",
                "2",
                "hi",
            ][..],
            "--compaction-budget-bytes was provided more than once",
        ),
    ] {
        let mut args = args.iter().copied().map(str::to_owned);
        let error = match Args::parse_with_env(&mut args, EnvArgs::default()) {
            Ok(_) => panic!("expected args error"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn reasoning_effort_rejected_outside_exec_and_bad_values() {
    for (args, expected) in [
        (
            &["run", "--reasoning-effort", "xlarge"][..],
            "--reasoning-effort is only supported with exec",
        ),
        (
            &["exec", "--reasoning-effort", "ultra", "hi"][..],
            "--reasoning-effort must be one of xsmall|small|medium|large|xlarge",
        ),
        (
            &[
                "exec",
                "--reasoning-effort",
                "small",
                "--reasoning-effort",
                "large",
                "hi",
            ][..],
            "--reasoning-effort was provided more than once",
        ),
    ] {
        let mut args = args.iter().copied().map(str::to_owned);
        let error = match Args::parse_with_env(&mut args, EnvArgs::default()) {
            Ok(_) => panic!("expected args error"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), expected);
    }
}
