use crate::provider_config::{ApiFamily, ProviderConfigRegistry};

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
        .any(|warning| warning.contains("owned by the OpenAI Chat Completions adapter")));
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
              "api_family": "anthropic_messages",
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
        .any(|warning| warning.contains("anthropic_messages")));
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
