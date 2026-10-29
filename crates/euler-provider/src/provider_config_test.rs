use crate::provider_config::{
    referenced_env_names, ApiFamily, CustomModelConfig, ProviderConfigRegistry,
};

#[test]
fn parses_custom_provider_without_resolving_secret_references() {
    let (registry, warnings) = ProviderConfigRegistry::with_json(
        r#"{
          "version": 1,
          "providers": {
            "local_openai": {
              "api_family": "openai_chat_completions",
              "base_url": "http://localhost:11434/v1",
              "api_key": "$LOCAL_OPENAI_API_KEY",
              "auth_header": true,
              "headers": {
                "x-extra-secret": "!op read 'op://vault/item/token'"
              },
              "default_model": "qwen3-coder",
              "models": [
                {
                  "id": "qwen3-coder",
                  "display_name": "Qwen 3 Coder",
                  "context_window_tokens": 262144,
                  "max_output_tokens": 32768,
                  "supports_tools": true,
                  "supports_reasoning": true,
                  "compat": {
                    "supports_developer_role": false,
                    "supports_stream_usage": false,
                    "max_tokens_field": "max_tokens",
                    "requires_tool_result_name": true,
                    "requires_assistant_after_tool_result": true,
                    "supports_strict_tools": false,
                    "reasoning": {
                      "request_format": "qwen_enable_thinking",
                      "effort_map": {
                        "minimal": "false",
                        "xhigh": "true"
                      },
                      "capture": "readable_or_summary"
                    }
                  }
                }
              ]
            }
          }
        }"#,
    );

    assert!(warnings.is_empty(), "{warnings:?}");
    let provider = registry.provider("local_openai").expect("provider");
    assert_eq!(provider.id, "local_openai");
    assert_eq!(provider.api_family, ApiFamily::OpenAiChatCompletions);
    assert_eq!(provider.api_key.as_deref(), Some("$LOCAL_OPENAI_API_KEY"));
    assert!(provider.auth_header);
    assert_eq!(
        provider.headers.get("x-extra-secret").map(String::as_str),
        Some("!op read 'op://vault/item/token'")
    );
    assert_eq!(provider.default_model.as_deref(), Some("qwen3-coder"));
    assert!(provider.default_model_error.is_none());

    let model = provider.models.get("qwen3-coder").expect("model");
    assert_eq!(model.display_name, "Qwen 3 Coder");
    assert_eq!(model.context_window_tokens, Some(262144));
    assert_eq!(model.max_output_tokens, Some(32768));
    assert_eq!(model.supports_tools, Some(true));
    assert_eq!(model.supports_reasoning, Some(true));
    let compat = model.compat.as_ref().expect("compat");
    assert_eq!(
        compat
            .get("max_tokens_field")
            .and_then(|value| value.as_str()),
        Some("max_tokens")
    );
    assert_eq!(
        compat
            .get("supports_developer_role")
            .and_then(|value| value.as_bool()),
        Some(false)
    );
    assert_eq!(
        compat
            .get("supports_stream_usage")
            .and_then(|value| value.as_bool()),
        Some(false)
    );
    assert_eq!(
        compat
            .get("requires_tool_result_name")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        compat
            .get("requires_assistant_after_tool_result")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        compat
            .get("supports_strict_tools")
            .and_then(|value| value.as_bool()),
        Some(false)
    );
    let reasoning = compat
        .get("reasoning")
        .and_then(|value| value.as_object())
        .expect("reasoning");
    assert_eq!(
        reasoning
            .get("request_format")
            .and_then(|value| value.as_str()),
        Some("qwen_enable_thinking")
    );
    assert_eq!(
        reasoning.get("capture").and_then(|value| value.as_str()),
        Some("readable_or_summary")
    );
    assert_eq!(
        reasoning
            .get("effort_map")
            .and_then(|value| value.get("minimal"))
            .and_then(|value| value.as_str()),
        Some("false")
    );
    assert_eq!(
        reasoning
            .get("effort_map")
            .and_then(|value| value.get("xhigh"))
            .and_then(|value| value.as_str()),
        Some("true")
    );
}

#[test]
fn rejects_reserved_and_malformed_provider_ids() {
    let (registry, warnings) = ProviderConfigRegistry::with_json(
        r#"{
          "providers": {
            "openai": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1",
              "models": [{"id": "custom"}]
            },
            "Local": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1",
              "models": [{"id": "custom"}]
            }
          }
        }"#,
    );

    assert_eq!(registry.providers().count(), 0);
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("built-in provider ids")));
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("lowercase ASCII")));
}

#[test]
fn validates_base_url_security_rules() {
    let (registry, warnings) = ProviderConfigRegistry::with_json(
        r#"{
          "providers": {
            "loopback": {
              "api_family": "openai_chat_completions",
              "base_url": "http://127.0.0.1:8080/v1",
              "models": [{"id": "ok"}]
            },
            "remote_http": {
              "api_family": "openai_chat_completions",
              "base_url": "http://example.com/v1",
              "models": [{"id": "bad"}]
            },
            "credentialed": {
              "api_family": "openai_chat_completions",
              "base_url": "https://user:pass@example.com/v1",
              "models": [{"id": "bad"}]
            },
            "query": {
              "api_family": "openai_chat_completions",
              "base_url": "https://example.com/v1?token=secret",
              "models": [{"id": "bad"}]
            },
            "fragment": {
              "api_family": "openai_chat_completions",
              "base_url": "https://example.com/v1#token",
              "models": [{"id": "bad"}]
            },
            "ipv6_loopback": {
              "api_family": "openai_chat_completions",
              "base_url": "http://[::1]:8080/v1",
              "models": [{"id": "ok"}]
            },
            "mapped_ipv6": {
              "api_family": "openai_chat_completions",
              "base_url": "http://[::ffff:127.0.0.1]/v1",
              "models": [{"id": "bad"}]
            },
            "empty_host": {
              "api_family": "openai_chat_completions",
              "base_url": "https://:443/v1",
              "models": [{"id": "bad"}]
            },
            "bad_port": {
              "api_family": "openai_chat_completions",
              "base_url": "https://example.com:abc/v1",
              "models": [{"id": "bad"}]
            },
            "space_host": {
              "api_family": "openai_chat_completions",
              "base_url": "https://exa mple.com/v1",
              "models": [{"id": "bad"}]
            }
          }
        }"#,
    );

    assert!(registry.provider("loopback").is_some());
    assert!(registry.provider("ipv6_loopback").is_some());
    assert!(registry.provider("remote_http").is_none());
    assert!(registry.provider("credentialed").is_none());
    assert!(registry.provider("query").is_none());
    assert!(registry.provider("fragment").is_none());
    assert!(registry.provider("mapped_ipv6").is_none());
    assert!(registry.provider("empty_host").is_none());
    assert!(registry.provider("bad_port").is_none());
    assert!(registry.provider("space_host").is_none());
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("non-loopback HTTP")));
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("embedded credentials")));
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("query strings or fragments")));
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("valid absolute http or https URL")));
}

#[test]
fn rejects_invalid_headers_and_authorization_conflict() {
    let (registry, warnings) = ProviderConfigRegistry::with_json(
        r#"{
          "providers": {
            "bad_name": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1",
              "headers": {"bad header": "value"},
              "models": [{"id": "custom"}]
            },
            "auth_conflict": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1",
              "api_key": "$KEY",
              "auth_header": true,
              "headers": {"Authorization": "$OTHER"},
              "models": [{"id": "custom"}]
            },
            "adapter_header": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1",
              "headers": {"Content-Type": "application/json"},
              "models": [{"id": "custom"}]
            }
          }
        }"#,
    );

    assert_eq!(registry.providers().count(), 0);
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("not a valid HTTP header name")));
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("conflicts with auth_header")));
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("owned by the provider adapter")));
}

#[test]
fn allows_manual_authorization_header_when_auth_header_is_false() {
    let (registry, warnings) = ProviderConfigRegistry::with_json(
        r#"{
          "providers": {
            "manual_auth": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1",
              "auth_header": false,
              "headers": {"Authorization": "$CUSTOM_AUTH"},
              "models": [{"id": "custom"}]
            }
          }
        }"#,
    );

    assert!(warnings.is_empty(), "{warnings:?}");
    let provider = registry.provider("manual_auth").expect("provider");
    assert_eq!(
        provider.headers.get("Authorization").map(String::as_str),
        Some("$CUSTOM_AUTH")
    );
}

#[test]
fn rejects_unsupported_api_family_and_empty_models() {
    let (registry, warnings) = ProviderConfigRegistry::with_json(
        r#"{
          "providers": {
            "bad_family": {
              "api_family": "google_gemini",
              "base_url": "https://proxy.example.com/v1",
              "models": [{"id": "custom"}]
            },
            "empty_models": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1",
              "models": []
            }
          }
        }"#,
    );

    assert_eq!(registry.providers().count(), 0);
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("api_family") && warning.contains("not supported")));
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("google_gemini")));
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("no valid models")));
}

#[test]
fn strips_leading_bom() {
    let (registry, warnings) = ProviderConfigRegistry::with_json(
        "\u{feff}{\"providers\":{\"local\":{\"api_family\":\"openai_chat_completions\",\"base_url\":\"https://proxy.example.com/v1\",\"models\":[{\"id\":\"custom\"}]}}}",
    );

    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(registry.provider("local").is_some());
}

#[test]
fn duplicate_models_are_last_valid_wins_and_default_error_is_kept() {
    let (registry, warnings) = ProviderConfigRegistry::with_json(
        r#"{
          "providers": {
            "local": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1",
              "default_model": "missing",
              "models": [
                {"id": "dup", "display_name": "First"},
                {"id": "dup", "display_name": "Second"}
              ]
            }
          }
        }"#,
    );

    let provider = registry.provider("local").expect("provider");
    let model = provider.models.get("dup").expect("model");
    assert_eq!(model.display_name, "Second");
    assert_eq!(provider.default_model, None);
    assert!(provider
        .default_model_error
        .as_ref()
        .is_some_and(|error| error.contains("providers.local.default_model")));
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("last valid descriptor wins")));
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("providers.local.default_model")));
}

#[test]
fn malformed_json_and_duplicate_provider_keys_degrade_to_empty_registry() {
    let (registry, warnings) = ProviderConfigRegistry::with_json("{");
    assert_eq!(registry.providers().count(), 0);
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("not valid JSON")));

    let (registry, warnings) = ProviderConfigRegistry::with_json(
        r#"{
          "providers": {
            "local": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1",
              "models": [{"id": "first"}]
            },
            "local": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1",
              "models": [{"id": "second"}]
            }
          }
        }"#,
    );
    assert_eq!(registry.providers().count(), 0);
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("duplicate provider key")));
}

#[test]
fn unknown_or_invalid_compatibility_fields_warn_without_dropping_model() {
    let (registry, warnings) = ProviderConfigRegistry::with_json(
        r#"{
          "providers": {
            "local": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1",
              "models": [
                {
                  "id": "custom",
                  "compat": {
                    "unknown": true,
                    "max_tokens_field": "wrong",
                    "reasoning": {
                      "request_format": "openrouter_reasoning",
                      "capture": "opaque_only",
                      "effort_map": {
                        "medium": "medium",
                        "too_much": "nope"
                      }
                    }
                  }
                }
              ]
            }
          }
        }"#,
    );

    let model = registry
        .provider("local")
        .and_then(|provider| provider.models.get("custom"))
        .expect("model");
    let reasoning = model
        .compat
        .as_ref()
        .and_then(|compat| compat.get("reasoning"))
        .and_then(|value| value.as_object())
        .expect("reasoning");
    assert_eq!(
        reasoning
            .get("request_format")
            .and_then(|value| value.as_str()),
        Some("openrouter_reasoning")
    );
    assert_eq!(
        reasoning.get("capture").and_then(|value| value.as_str()),
        Some("opaque_only")
    );
    assert_eq!(
        reasoning
            .get("effort_map")
            .and_then(|value| value.get("medium"))
            .and_then(|value| value.as_str()),
        Some("medium")
    );
    assert!(
        reasoning
            .get("effort_map")
            .and_then(|value| value.get("too_much"))
            .is_some(),
        "raw compat preserves unconsumed fields for the next slice"
    );
    assert!(warnings.iter().any(|warning| warning.contains("unknown")));
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("max_tokens_field") && warning.contains("not supported")));
    assert!(!warnings
        .iter()
        .any(|warning| warning.contains("max_tokens_field") && warning.contains("wrong")));
    assert!(warnings.iter().any(|warning| warning.contains("too_much")));
}

#[test]
fn invalid_active_compat_types_warn_without_dropping_model() {
    let (registry, warnings) = ProviderConfigRegistry::with_json(
        r#"{
          "providers": {
            "local": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1",
              "models": [
                {
                  "id": "custom",
                  "compat": {
                    "supports_stream_usage": "false",
                    "reasoning": {
                      "request_format": "openrouter_reasoning",
                      "capture": 123,
                      "effort_map": {
                        "minimal": false
                      }
                    }
                  }
                }
              ]
            }
          }
        }"#,
    );

    let model = registry
        .provider("local")
        .and_then(|provider| provider.models.get("custom"))
        .expect("model");

    assert!(model.compat.is_some());
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("supports_stream_usage") && warning.contains("boolean")));
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("capture") && warning.contains("not a string")));
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("minimal") && warning.contains("not a string")));
}

#[test]
fn reserved_qwen_chat_template_warns_without_echoing_value() {
    let (_registry, warnings) = ProviderConfigRegistry::with_json(
        r#"{
          "providers": {
            "local": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1",
              "models": [
                {
                  "id": "custom",
                  "compat": {
                    "reasoning": {
                      "request_format": "qwen_chat_template"
                    }
                  }
                }
              ]
            }
          }
        }"#,
    );

    let warning = warnings
        .iter()
        .find(|warning| warning.contains("request_format"))
        .expect("request_format warning");
    assert!(warning.contains("not supported"));
    assert!(!warning.contains("qwen_chat_template"));
}

#[test]
fn default_model_without_models_becomes_the_only_model_with_no_metadata() {
    let (registry, warnings) = ProviderConfigRegistry::with_json(
        r#"{
          "providers": {
            "local": {
              "api_family": "openai_chat_completions",
              "base_url": "http://localhost:11434/v1",
              "default_model": "qwen3:32b"
            }
          }
        }"#,
    );

    assert!(warnings.is_empty(), "{warnings:?}");
    let provider = registry.provider("local").expect("local provider");
    assert_eq!(provider.default_model.as_deref(), Some("qwen3:32b"));
    assert_eq!(provider.default_model_error, None);
    let models = provider.models.values().collect::<Vec<_>>();
    assert_eq!(
        models,
        [&CustomModelConfig {
            id: "qwen3:32b".to_owned(),
            display_name: "qwen3:32b".to_owned(),
            api_family: ApiFamily::OpenAiChatCompletions,
            context_window_tokens: None,
            max_output_tokens: None,
            supports_tools: None,
            supports_reasoning: None,
            compat: None,
        }]
    );
}

#[test]
fn missing_empty_or_non_array_models_warn_with_the_fix() {
    let (registry, warnings) = ProviderConfigRegistry::with_json(
        r#"{
          "providers": {
            "no_models": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1"
            },
            "non_string_default": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1",
              "default_model": 42
            },
            "invalid_default": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1",
              "default_model": " padded "
            },
            "empty_models": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1",
              "default_model": "custom",
              "models": []
            },
            "object_models": {
              "api_family": "openai_chat_completions",
              "base_url": "https://proxy.example.com/v1",
              "default_model": "custom",
              "models": {"custom": {}}
            }
          }
        }"#,
    );

    assert_eq!(registry.providers().count(), 0);
    for id in [
        "no_models",
        "non_string_default",
        "invalid_default",
        "empty_models",
        "object_models",
    ] {
        let warning = warnings
            .iter()
            .find(|warning| warning.starts_with(&format!("ignored provider `{id}` because")))
            .unwrap_or_else(|| panic!("warning for {id}: {warnings:?}"));
        assert!(
            warning.contains("list models as") && warning.contains("set \"default_model\""),
            "{warning}"
        );
    }
}

#[test]
fn headless_guide_custom_provider_examples_parse_without_warnings() {
    let guide = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/guides/headless.md"
    ))
    .expect("headless guide");
    let section = guide
        .split("## Custom providers")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .expect("custom providers section");
    let examples = section
        .split("```json")
        .skip(1)
        .filter_map(|rest| rest.split("```").next())
        .collect::<Vec<_>>();
    assert_eq!(examples.len(), 2, "providers.json examples");

    let mut ids = Vec::new();
    let mut messages_models = 0;
    let mut responses_models = 0;
    for example in examples {
        let (registry, warnings) = ProviderConfigRegistry::with_json(example);
        assert!(warnings.is_empty(), "{warnings:?}");
        for provider in registry.providers() {
            ids.push(provider.id.clone());
            let default_model = provider.default_model.as_deref().expect("default model");
            assert!(provider.models.contains_key(default_model));
            messages_models += provider
                .models
                .values()
                .filter(|model| model.api_family == ApiFamily::AnthropicMessages)
                .count();
            responses_models += provider
                .models
                .values()
                .filter(|model| model.api_family == ApiFamily::OpenAiResponses)
                .count();
        }
    }
    assert_eq!(ids.len(), 3);
    assert!(ids.contains(&"gateway".to_owned()) && ids.contains(&"local".to_owned()));
    assert_eq!(messages_models, 1);
    assert_eq!(responses_models, 1);
}

#[test]
fn model_api_family_overrides_the_provider_family() {
    let (registry, warnings) = ProviderConfigRegistry::with_json(
        r#"{
          "providers": {
            "gateway": {
              "api_family": "openai_chat_completions",
              "base_url": "https://gateway.example.com/v1",
              "models": [
                { "id": "chat-model" },
                { "id": "messages-model", "api_family": "anthropic_messages" },
                { "id": "responses-model", "api_family": "openai_responses" }
              ]
            },
            "responses_only": {
              "api_family": "openai_responses",
              "base_url": "https://gateway.example.com/v1/responses",
              "models": [{ "id": "responses-default" }]
            },
            "messages_only": {
              "api_family": "anthropic_messages",
              "base_url": "https://gateway.example.com",
              "headers": {
                "x-api-key": "$ANTHROPIC_API_KEY",
                "anthropic-version": "2023-06-01"
              },
              "default_model": "messages-default"
            }
          }
        }"#,
    );

    assert!(warnings.is_empty(), "{warnings:?}");
    let gateway = registry.provider("gateway").expect("gateway provider");
    assert_eq!(
        gateway.model_api_family("chat-model"),
        ApiFamily::OpenAiChatCompletions
    );
    assert_eq!(
        gateway.model_api_family("messages-model"),
        ApiFamily::AnthropicMessages
    );
    assert_eq!(
        gateway.model_api_family("responses-model"),
        ApiFamily::OpenAiResponses
    );
    assert_eq!(
        gateway.model_api_family("unlisted-model"),
        ApiFamily::OpenAiChatCompletions
    );
    let responses_only = registry
        .provider("responses_only")
        .expect("responses provider");
    assert_eq!(responses_only.api_family, ApiFamily::OpenAiResponses);
    assert_eq!(
        responses_only.model_api_family("responses-default"),
        ApiFamily::OpenAiResponses
    );
    let messages_only = registry
        .provider("messages_only")
        .expect("messages provider");
    assert_eq!(messages_only.api_family, ApiFamily::AnthropicMessages);
    assert_eq!(
        messages_only.headers.keys().collect::<Vec<_>>(),
        ["anthropic-version", "x-api-key"]
    );
    assert_eq!(
        messages_only.model_api_family("messages-default"),
        ApiFamily::AnthropicMessages
    );
}

#[test]
fn provider_whose_full_endpoint_base_url_serves_another_family_is_ignored() {
    let (registry, warnings) = ProviderConfigRegistry::with_json(
        r#"{
          "providers": {
            "messages_on_chat_endpoint": {
              "api_family": "anthropic_messages",
              "base_url": "https://gateway.example.com/v1/chat/completions",
              "default_model": "messages-model"
            },
            "chat_on_messages_endpoint": {
              "api_family": "openai_chat_completions",
              "base_url": "https://gateway.example.com/v1/messages",
              "models": [{ "id": "messages-model", "api_family": "anthropic_messages" }]
            },
            "messages_on_messages_endpoint": {
              "api_family": "anthropic_messages",
              "base_url": "https://gateway.example.com/v1/messages",
              "default_model": "messages-model"
            },
            "chat_on_responses_endpoint": {
              "api_family": "openai_chat_completions",
              "base_url": "https://gateway.example.com/v1/responses",
              "default_model": "chat-model"
            },
            "responses_on_chat_endpoint": {
              "api_family": "openai_responses",
              "base_url": "https://gateway.example.com/v1/chat/completions",
              "default_model": "responses-model"
            }
          }
        }"#,
    );

    assert!(registry.provider("messages_on_messages_endpoint").is_some());
    assert!(registry.provider("messages_on_chat_endpoint").is_none());
    assert!(registry.provider("chat_on_messages_endpoint").is_none());
    assert!(registry.provider("chat_on_responses_endpoint").is_none());
    assert!(registry.provider("responses_on_chat_endpoint").is_none());
    for provider in [
        "messages_on_chat_endpoint",
        "chat_on_messages_endpoint",
        "chat_on_responses_endpoint",
        "responses_on_chat_endpoint",
    ] {
        let expected = format!(
            "ignored provider `{provider}` because base_url is a full endpoint for another api_family"
        );
        assert!(
            warnings.iter().any(|warning| warning.contains(&expected)),
            "missing `{expected}` in {warnings:?}"
        );
    }
}

#[test]
fn model_api_family_entries_that_cannot_be_routed_are_ignored_with_warnings() {
    let (registry, warnings) = ProviderConfigRegistry::with_json(
        r#"{
          "providers": {
            "gateway": {
              "api_family": "openai_chat_completions",
              "base_url": "https://gateway.example.com/v1/chat/completions",
              "models": [
                { "id": "chat-model" },
                { "id": "full-endpoint-mismatch", "api_family": "anthropic_messages" },
                { "id": "unknown-family", "api_family": "google_gemini" },
                { "id": "responses-mismatch", "api_family": "openai_responses" }
              ]
            },
            "messages_compat": {
              "api_family": "anthropic_messages",
              "base_url": "https://gateway.example.com",
              "models": [
                { "id": "messages-model", "compat": { "supports_developer_role": false } }
              ]
            },
            "responses_compat": {
              "api_family": "openai_responses",
              "base_url": "https://gateway.example.com/v1",
              "models": [
                { "id": "responses-model", "compat": { "supports_developer_role": false } }
              ]
            }
          }
        }"#,
    );

    let gateway = registry.provider("gateway").expect("gateway provider");
    assert_eq!(
        gateway
            .models
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["chat-model"]
    );
    let compat_model = &registry
        .provider("messages_compat")
        .expect("messages provider")
        .models["messages-model"];
    assert_eq!(compat_model.compat, None);
    let responses_compat_model = &registry
        .provider("responses_compat")
        .expect("responses provider")
        .models["responses-model"];
    assert_eq!(responses_compat_model.compat, None);
    for expected in [
        "model #1 because the provider base_url is a full endpoint for another api_family",
        "model #2 api_family `google_gemini` because it is not supported",
        "model #3 because the provider base_url is a full endpoint for another api_family",
        "provider `messages_compat` model #0 compat because it applies only to openai_chat_completions",
        "provider `responses_compat` model #0 compat because it applies only to openai_chat_completions",
    ] {
        assert!(
            warnings.iter().any(|warning| warning.contains(expected)),
            "missing `{expected}` in {warnings:?}"
        );
    }
}

#[test]
fn referenced_env_names_lists_env_references_in_api_keys_and_headers() {
    let contents = r#"{
      "version": 1,
      "providers": {
        "proxy": {
          "api_family": "openai_chat_completions",
          "base_url": "https://litellm.example.com/v1",
          "api_key": "$LITELLM_API_KEY",
          "auth_header": true,
          "headers": {
            "x-team": "${PROXY_TEAM}",
            "x-command": "!op read 'op://vault/item/token'",
            "x-literal": "$$NOT_A_REFERENCE"
          },
          "default_model": "m",
          "models": [{"id": "m"}]
        }
      }
    }"#;
    let (_, warnings) = ProviderConfigRegistry::with_json(contents);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        referenced_env_names(contents)
            .into_iter()
            .collect::<Vec<_>>(),
        ["LITELLM_API_KEY", "PROXY_TEAM"]
    );
    assert_eq!(
        referenced_env_names(&format!("\u{feff}{contents}")),
        referenced_env_names(contents)
    );
}

#[test]
fn referenced_env_names_includes_provider_entries_load_ignores() {
    let contents = r#"{
      "version": 1,
      "providers": {
        "no_family": {
          "base_url": "https://litellm.example.com/v1",
          "api_key": "$NO_FAMILY_KEY"
        },
        "bad_header": {
          "api_family": "openai_chat_completions",
          "base_url": "https://litellm.example.com/v1",
          "headers": {"bad header": "${BAD_HEADER_TOKEN}"},
          "models": [{"id": "m"}]
        },
        "not_an_object": "$NOT_A_PROVIDER"
      }
    }"#;
    let (registry, _) = ProviderConfigRegistry::with_json(contents);
    assert!(registry.providers.is_empty(), "{registry:?}");
    assert_eq!(
        referenced_env_names(contents)
            .into_iter()
            .collect::<Vec<_>>(),
        ["BAD_HEADER_TOKEN", "NO_FAMILY_KEY"]
    );
    assert!(referenced_env_names("not json").is_empty());
}

#[test]
fn referenced_env_names_includes_every_entry_of_a_repeated_provider_id() {
    let contents = r#"{
      "version": 1,
      "providers": {
        "proxy": {"api_key": "$FIRST_PROXY_KEY"},
        "proxy": {"api_key": "$SECOND_PROXY_KEY"}
      }
    }"#;
    let (registry, warnings) = ProviderConfigRegistry::with_json(contents);
    assert!(registry.providers.is_empty(), "{registry:?}");
    assert!(
        warnings.iter().any(|warning| warning.contains("duplicate")),
        "{warnings:?}"
    );
    assert_eq!(
        referenced_env_names(contents)
            .into_iter()
            .collect::<Vec<_>>(),
        ["FIRST_PROXY_KEY", "SECOND_PROXY_KEY"]
    );
    let repeated_providers_key = r#"{
      "providers": {"proxy": {"api_key": "$FIRST_PROXY_KEY"}},
      "providers": "not an object",
      "providers": [{"api_key": "$IN_AN_ARRAY"}],
      "providers": null,
      "providers": {"other": {"headers": {"x-team": "${TEAM_TOKEN}"}}}
    }"#;
    assert_eq!(
        referenced_env_names(repeated_providers_key)
            .into_iter()
            .collect::<Vec<_>>(),
        ["FIRST_PROXY_KEY", "TEAM_TOKEN"]
    );
    assert!(referenced_env_names(r#"[{"proxy": {"api_key": "$KEY"}}]"#).is_empty());
}
