use super::*;
use std::sync::{Arc, Barrier};
use std::thread;
use tempfile::TempDir;
fn auth_path(temp: &TempDir) -> PathBuf {
    temp.path().join(".euler").join("auth.json")
}
fn api_key(value: &str) -> Credential {
    Credential::ApiKey {
        key: SecretString::new(value),
    }
}
fn oauth(access: &str, refresh: &str, expires: u64) -> Credential {
    Credential::OAuth {
        access: SecretString::new(access),
        refresh: SecretString::new(refresh),
        expires,
        account_id: Some("acct-1".to_owned()),
    }
}
fn refreshed(access: &str, refresh: Option<&str>, expires: u64) -> RefreshedOAuthCredential {
    RefreshedOAuthCredential {
        access: SecretString::new(access),
        refresh: refresh.map(SecretString::new),
        expires,
        account_id: Some("acct-refreshed".to_owned()),
    }
}
fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("read auth file")).expect("json")
}
#[test]
fn save_and_load_round_trip_with_api_key_and_oauth_credentials() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("openrouter", api_key("sk-test"))
        .expect("set api");
    storage
        .set("chatgpt", oauth("access-token", "refresh-token", u64::MAX))
        .expect("set oauth");
    let loaded = AuthStorage::new(&path).expect("reload");
    assert_eq!(loaded.get("openrouter"), Some(api_key("sk-test")));
    assert_eq!(
        loaded.get("chatgpt"),
        Some(oauth("access-token", "refresh-token", u64::MAX))
    );
}
#[cfg(unix)]
#[test]
fn permission_bits_on_directory_and_file() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage.set("openrouter", api_key("sk-test")).expect("set");
    let dir_mode = fs::metadata(path.parent().expect("parent"))
        .expect("dir metadata")
        .permissions()
        .mode()
        & 0o777;
    let file_mode = fs::metadata(&path)
        .expect("file metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700);
    assert_eq!(file_mode, 0o600);
}
#[cfg(unix)]
#[test]
fn preexisting_leaf_directory_permissions_are_repaired_on_write() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let dir = path.parent().expect("parent");
    fs::create_dir_all(dir).expect("create dir");
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).expect("loosen dir");
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage.set("openrouter", api_key("sk-test")).expect("set");
    let dir_mode = fs::metadata(dir)
        .expect("dir metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700);
}
#[test]
fn atomic_write_does_not_leave_partial_files() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage.set("openrouter", api_key("sk-test")).expect("set");
    let partials = fs::read_dir(path.parent().expect("parent"))
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(TEMP_SUFFIX))
        .collect::<Vec<_>>();
    assert!(partials.is_empty(), "left partial files: {partials:?}");
    assert!(read_json(&path).is_object());
}
#[test]
fn unknown_fields_and_provider_entries_are_preserved() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
    fs::write(
        &path,
        r#"{
  "version": 1,
  "future_top": {"keep": true},
  "providers": {
    "openrouter": {
      "type": "api_key",
      "key": "old-key",
      "future_credential_field": "keep-me"
    },
    "future-provider": {
      "type": "future",
      "opaque": {"keep": "yes"}
    }
  }
}"#,
    )
    .expect("seed");
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage.set("openrouter", api_key("new-key")).expect("set");
    let json = read_json(&path);
    assert_eq!(json["future_top"]["keep"], Value::Bool(true));
    assert_eq!(
        json["providers"]["openrouter"]["future_credential_field"],
        Value::from("keep-me")
    );
    assert_eq!(
        json["providers"]["future-provider"]["opaque"]["keep"],
        Value::from("yes")
    );
    assert_eq!(
        json["providers"]["openrouter"]["key"],
        Value::from("new-key")
    );
}
#[test]
fn forward_version_refuses_mutation() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
    fs::write(
        &path,
        r#"{"version":2,"providers":{"openrouter":{"type":"api_key","key":"kept"}}}"#,
    )
    .expect("seed");
    let mut storage = AuthStorage::new(&path).expect("storage");
    assert_eq!(storage.get("openrouter"), Some(api_key("kept")));
    let error = storage
        .set("anthropic", api_key("new"))
        .expect_err("forward version refuses mutation");
    assert!(matches!(error, AuthError::Invalid(_)));
    let json = read_json(&path);
    assert!(json["providers"].get("anthropic").is_none());
}
#[test]
fn runtime_override_takes_precedence_over_stored_and_env() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::with_env_lookup(&path, |name| {
        (name == "OPENROUTER_API_KEY").then(|| "env-key".to_owned())
    })
    .expect("storage");
    storage
        .set("openrouter", api_key("stored-key"))
        .expect("set");
    storage.set_runtime_api_key("openrouter", "runtime-key".to_owned());
    let resolved = storage
        .resolve_api_key("openrouter", Some("OPENROUTER_API_KEY"))
        .expect("resolved");
    assert_eq!(resolved.expose_secret(), "runtime-key");
    assert_eq!(
        storage.auth_status("openrouter", Some("OPENROUTER_API_KEY")),
        AuthStatus {
            configured: true,
            source: AuthSource::Runtime,
            state: AuthState::Valid,
        }
    );
}
#[test]
fn auth_file_source_is_reported_for_explicit_auth_file_storage() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
    fs::write(
        &path,
        r#"{"version":1,"providers":{"openrouter":{"type":"api_key","key":"auth-file-key"}}}"#,
    )
    .expect("seed");
    let mut storage = AuthStorage::new_auth_file(&path).expect("storage");
    assert_eq!(
        storage.auth_status("openrouter", None),
        AuthStatus {
            configured: true,
            source: AuthSource::AuthFile,
            state: AuthState::Valid,
        }
    );
    assert!(matches!(
        storage.set("openrouter", api_key("new-key")),
        Err(AuthError::Invalid(_))
    ));
}
#[test]
fn stored_env_reference_resolves_at_request_time() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::with_env_lookup(&path, |name| {
        (name == "OPENROUTER_API_KEY").then(|| "resolved-env-key".to_owned())
    })
    .expect("storage");
    storage
        .set("openrouter", api_key("$OPENROUTER_API_KEY"))
        .expect("set");
    assert_eq!(
        storage
            .resolve_api_key("openrouter", Some("FALLBACK"))
            .expect("resolved")
            .expose_secret(),
        "resolved-env-key"
    );
}

#[test]
fn stored_braced_env_reference_resolves_at_request_time() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::with_env_lookup(&path, |name| {
        (name == "OPENROUTER_API_KEY").then(|| "resolved-braced-key".to_owned())
    })
    .expect("storage");
    storage
        .set("openrouter", api_key("${OPENROUTER_API_KEY}"))
        .expect("set");
    assert_eq!(
        storage
            .resolve_api_key("openrouter", Some("FALLBACK"))
            .expect("resolved")
            .expose_secret(),
        "resolved-braced-key"
    );
}

#[test]
fn stored_braced_env_name_construction_resolves_final_env_key() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::with_env_lookup(&path, |name| match name {
        "KEY_PREFIX" => Some("OPENROUTER".to_owned()),
        "OPENROUTER_API_KEY" => Some("resolved-dynamic-key".to_owned()),
        _ => None,
    })
    .expect("storage");
    storage
        .set("openrouter", api_key("${KEY_PREFIX}_API_KEY"))
        .expect("set");
    assert_eq!(
        storage
            .resolve_api_key("openrouter", None)
            .expect("resolved")
            .expose_secret(),
        "resolved-dynamic-key"
    );
}

#[test]
fn stored_braced_env_name_construction_missing_prefix_is_unresolved() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::with_env_lookup(&path, |name| {
        (name == "OPENROUTER_API_KEY").then(|| "resolved-dynamic-key".to_owned())
    })
    .expect("storage");
    storage
        .set("openrouter", api_key("${KEY_PREFIX}_API_KEY"))
        .expect("set");
    assert!(
        storage.resolve_api_key("openrouter", None).is_none(),
        "missing KEY_PREFIX leaves the stored reference unresolved"
    );
}

#[test]
fn stored_braced_env_name_construction_missing_final_key_is_unresolved() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::with_env_lookup(&path, |name| {
        (name == "KEY_PREFIX").then(|| "OPENROUTER".to_owned())
    })
    .expect("storage");
    storage
        .set("openrouter", api_key("${KEY_PREFIX}_API_KEY"))
        .expect("set");
    assert!(
        storage.resolve_api_key("openrouter", None).is_none(),
        "missing OPENROUTER_API_KEY leaves the stored reference unresolved"
    );
}

#[test]
fn stored_escaped_env_reference_remains_literal() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::with_env_lookup(&path, |_| {
        panic!("escaped literal must not consult environment")
    })
    .expect("storage");
    storage
        .set("openrouter", api_key("$$OPENROUTER_API_KEY"))
        .expect("set");
    assert_eq!(
        storage
            .resolve_api_key("openrouter", None)
            .expect("literal")
            .expose_secret(),
        "$OPENROUTER_API_KEY"
    );
}

#[test]
fn stored_escaped_command_reference_remains_literal() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::with_env_lookup(&path, |_| {
        panic!("escaped command literal must not consult environment")
    })
    .expect("storage");
    storage
        .set("openrouter", api_key("$!op read credential"))
        .expect("set");
    assert_eq!(
        storage
            .resolve_api_key("openrouter", None)
            .expect("literal")
            .expose_secret(),
        "!op read credential"
    );
}

#[test]
fn stored_command_reference_is_not_resolved_by_status_or_storage() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::with_env_lookup(&path, |name| {
        (name == "OPENROUTER_API_KEY").then(|| "fallback-env-key".to_owned())
    })
    .expect("storage");
    storage
        .set("openrouter", api_key("!op read credential"))
        .expect("set");
    assert_eq!(
        storage.auth_status("openrouter", Some("OPENROUTER_API_KEY")),
        AuthStatus {
            configured: true,
            source: AuthSource::Stored,
            state: AuthState::Valid,
        }
    );
    assert_eq!(
        storage
            .resolve_api_key("openrouter", Some("OPENROUTER_API_KEY"))
            .expect("fallback")
            .expose_secret(),
        "fallback-env-key"
    );
}

#[test]
fn env_fallback_precedence() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::with_env_lookup(&path, |name| {
        (name == "ANTHROPIC_API_KEY").then(|| "env-key".to_owned())
    })
    .expect("storage");
    assert_eq!(
        storage
            .resolve_api_key("anthropic", Some("ANTHROPIC_API_KEY"))
            .expect("env")
            .expose_secret(),
        "env-key"
    );
    storage
        .set("anthropic", api_key("stored-key"))
        .expect("set");
    assert_eq!(
        storage
            .resolve_api_key("anthropic", Some("ANTHROPIC_API_KEY"))
            .expect("stored")
            .expose_secret(),
        "stored-key"
    );
}
#[test]
fn auth_status_does_not_expose_values() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("chatgpt", oauth("access-secret", "refresh-secret", 1))
        .expect("set");
    let status = storage.auth_status("chatgpt", None);
    let formatted = format!("{status:?}");
    assert_eq!(
        status,
        AuthStatus {
            configured: true,
            source: AuthSource::Stored,
            state: AuthState::ExpiredRefreshable,
        }
    );
    assert!(!formatted.contains("access-secret"));
    assert!(!formatted.contains("refresh-secret"));
}

#[test]
fn oauth_refresh_triggers_inside_safety_margin_and_writes_rotation() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("chatgpt", oauth("old-access", "old-refresh", 130_000))
        .expect("seed");
    let mut callback_seen_refresh = None;

    let credential = storage
        .refresh_oauth_if_needed("chatgpt", 100_000, |refresh| {
            callback_seen_refresh = Some(refresh.expose_secret().to_owned());
            Ok(refreshed("new-access", Some("new-refresh"), 200_000))
        })
        .expect("refresh");

    assert_eq!(callback_seen_refresh.as_deref(), Some("old-refresh"));
    assert_eq!(
        credential,
        Credential::OAuth {
            access: SecretString::new("new-access"),
            refresh: SecretString::new("new-refresh"),
            expires: 200_000,
            account_id: Some("acct-refreshed".to_owned()),
        }
    );
    let json = read_json(&path);
    assert_eq!(json["providers"]["chatgpt"]["access"], "new-access");
    assert_eq!(json["providers"]["chatgpt"]["refresh"], "new-refresh");
    assert_eq!(json["providers"]["chatgpt"]["expires"], 200_000);
}

#[test]
fn oauth_refresh_skips_when_reread_credential_is_fresh() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("chatgpt", oauth("stale-access", "stale-refresh", 110_000))
        .expect("seed stale");

    let mut second = AuthStorage::new(&path).expect("second storage");
    second
        .set("chatgpt", oauth("fresh-access", "fresh-refresh", 200_000))
        .expect("seed fresh");

    let credential = storage
        .refresh_oauth_if_needed("chatgpt", 100_000, |_refresh| {
            panic!("fresh reread must not call refresh callback")
        })
        .expect("fresh reread");

    assert_eq!(credential, oauth("fresh-access", "fresh-refresh", 200_000));
}

#[test]
fn oauth_refresh_callback_error_redacts_refresh_token() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("chatgpt", oauth("old-access", "refresh-secret", 100_000))
        .expect("seed");

    let error = storage
        .refresh_oauth_if_needed("chatgpt", 100_000, |refresh| {
            assert_eq!(refresh.expose_secret(), "refresh-secret");
            Err(AuthError::Invalid(format!(
                "provider echoed {}",
                refresh.expose_secret()
            )))
        })
        .expect_err("refresh failure");
    let formatted = format!("{error:?} {error}");

    assert!(!formatted.contains("refresh-secret"));
    assert_eq!(
        storage.get("chatgpt"),
        Some(oauth("old-access", "refresh-secret", 100_000))
    );
}

#[test]
fn oauth_refresh_without_rotation_preserves_existing_refresh_token() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("chatgpt", oauth("old-access", "old-refresh", 100_000))
        .expect("seed");

    storage
        .refresh_oauth_if_needed("chatgpt", 100_000, |_refresh| {
            Ok(refreshed("new-access", None, 200_000))
        })
        .expect("refresh");

    assert_eq!(
        storage.get("chatgpt"),
        Some(Credential::OAuth {
            access: SecretString::new("new-access"),
            refresh: SecretString::new("old-refresh"),
            expires: 200_000,
            account_id: Some("acct-refreshed".to_owned()),
        })
    );
}

#[test]
fn oauth_refresh_failure_leaves_file_intact() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("chatgpt", oauth("old-access", "old-refresh", 100_000))
        .expect("seed");
    let before = fs::read_to_string(&path).expect("before");

    let error = storage
        .refresh_oauth_if_needed("chatgpt", 100_000, |_refresh| Err(AuthError::RefreshFailed))
        .expect_err("refresh failure");

    assert!(matches!(error, AuthError::RefreshFailed));
    assert_eq!(fs::read_to_string(&path).expect("after"), before);
    assert_eq!(
        storage.get("chatgpt"),
        Some(oauth("old-access", "old-refresh", 100_000))
    );
}

#[test]
fn oauth_refresh_failure_updates_memory_to_locked_reread() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("chatgpt", oauth("first-access", "first-refresh", 100_000))
        .expect("seed first");

    let mut second = AuthStorage::new(&path).expect("second storage");
    second
        .set("chatgpt", oauth("reread-access", "reread-refresh", 120_000))
        .expect("seed reread");
    let before = fs::read_to_string(&path).expect("before");

    let error = storage
        .refresh_oauth_if_needed("chatgpt", 100_000, |refresh| {
            assert_eq!(refresh.expose_secret(), "reread-refresh");
            Err(AuthError::RefreshFailed)
        })
        .expect_err("refresh failure");

    assert!(matches!(error, AuthError::RefreshFailed));
    assert_eq!(fs::read_to_string(&path).expect("after"), before);
    assert_eq!(
        storage.get("chatgpt"),
        Some(oauth("reread-access", "reread-refresh", 120_000))
    );
}

#[test]
fn oauth_refresh_rejects_immediately_expired_refreshed_credential() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("chatgpt", oauth("old-access", "old-refresh", 100_000))
        .expect("seed");
    let before = fs::read_to_string(&path).expect("before");

    let error = storage
        .refresh_oauth_if_needed("chatgpt", 100_000, |_refresh| {
            Ok(refreshed("new-access", Some("new-refresh"), 130_000))
        })
        .expect_err("immediately expired refresh");
    let formatted = format!("{error:?} {error}");

    assert!(matches!(error, AuthError::RefreshFailed));
    assert!(!formatted.contains("new-access"));
    assert!(!formatted.contains("new-refresh"));
    assert_eq!(fs::read_to_string(&path).expect("after"), before);
    assert_eq!(
        storage.get("chatgpt"),
        Some(oauth("old-access", "old-refresh", 100_000))
    );
}

#[test]
fn oauth_refresh_preserves_unknown_fields_and_providers() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
    fs::write(
        &path,
        r#"{
  "version": 1,
  "top_future": "keep",
  "providers": {
    "chatgpt": {
      "type": "oauth",
      "access": "old-access",
      "refresh": "old-refresh",
      "expires": 100000,
      "account_id": "acct-old",
      "credential_future": {"keep": true}
    },
    "future-provider": {
      "type": "future",
      "opaque": {"keep": "yes"}
    }
  }
}"#,
    )
    .expect("seed");
    let mut storage = AuthStorage::new(&path).expect("storage");

    storage
        .refresh_oauth_if_needed("chatgpt", 100_000, |_refresh| {
            Ok(refreshed("new-access", Some("new-refresh"), 200_000))
        })
        .expect("refresh");

    let json = read_json(&path);
    assert_eq!(json["top_future"], "keep");
    assert_eq!(
        json["providers"]["chatgpt"]["credential_future"]["keep"],
        true
    );
    assert_eq!(
        json["providers"]["future-provider"]["opaque"]["keep"],
        "yes"
    );
    assert_eq!(json["providers"]["chatgpt"]["access"], "new-access");
}

#[test]
fn explicit_auth_file_refuses_oauth_refresh() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("chatgpt", oauth("old-access", "old-refresh", 100_000))
        .expect("seed");
    let mut auth_file = AuthStorage::new_auth_file(&path).expect("auth-file storage");

    let error = auth_file
        .refresh_oauth_if_needed("chatgpt", 100_000, |_refresh| {
            panic!("read-only auth-file must not refresh")
        })
        .expect_err("read-only refresh");

    assert!(matches!(error, AuthError::Invalid(_)));
    assert_eq!(
        read_json(&path)["providers"]["chatgpt"]["access"],
        "old-access"
    );
}

#[test]
fn file_locking_concurrent_writes_do_not_corrupt_file() {
    let temp = TempDir::new().expect("temp dir");
    let path = Arc::new(auth_path(&temp));
    let writers = 8;
    let barrier = Arc::new(Barrier::new(writers));
    let mut handles = Vec::new();
    for index in 0..writers {
        let path = Arc::clone(&path);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let mut storage = AuthStorage::new(path.as_path()).expect("storage");
            barrier.wait();
            storage
                .set(
                    &format!("provider-{index}"),
                    api_key(&format!("key-{index}")),
                )
                .expect("set");
        }));
    }
    for handle in handles {
        handle.join().expect("thread");
    }
    let loaded = AuthStorage::new(path.as_path()).expect("reload");
    assert_eq!(loaded.list().len(), writers);
    for index in 0..writers {
        assert_eq!(
            loaded.get(&format!("provider-{index}")),
            Some(api_key(&format!("key-{index}")))
        );
    }
    assert!(read_json(path.as_path()).is_object());
}
#[test]
fn missing_file_is_treated_as_empty() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let storage = AuthStorage::new(&path).expect("storage");
    assert_eq!(storage.list(), Vec::<String>::new());
    assert!(!storage.has("openrouter"));
    assert_eq!(
        storage.auth_status("openrouter", None),
        AuthStatus {
            configured: false,
            source: AuthSource::Missing,
            state: AuthState::Missing,
        }
    );
}

#[test]
fn contains_provider_entry_reports_malformed_entries_without_parsing_as_credentials() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
    fs::write(
        &path,
        r#"{"version":1,"providers":{"anthropic":{"type":"api_key"}}}"#,
    )
    .expect("seed");

    let storage = AuthStorage::new(&path).expect("storage");

    assert!(storage.contains_provider_entry("anthropic"));
    assert!(!storage.has("anthropic"));
    assert!(storage.get("anthropic").is_none());
}

#[test]
fn secret_string_redacts_in_debug_and_display() {
    let secret = SecretString::new("sk-live-secret");
    assert!(!format!("{secret:?}").contains("sk-live-secret"));
    assert!(!format!("{secret}").contains("sk-live-secret"));
    assert!(format!("{secret}").contains("[redacted]"));
}
#[test]
fn credential_debug_redacts_secrets() {
    let credential = oauth("access-secret", "refresh-secret", u64::MAX);
    let formatted = format!("{credential:?}");
    assert!(!formatted.contains("access-secret"));
    assert!(!formatted.contains("refresh-secret"));
    assert!(formatted.contains("expires"));
}

#[test]
fn refreshed_oauth_credential_debug_redacts_secrets() {
    let credential = refreshed("access-secret", Some("refresh-secret"), u64::MAX);
    let formatted = format!("{credential:?}");
    assert!(!formatted.contains("access-secret"));
    assert!(!formatted.contains("refresh-secret"));
    assert!(formatted.contains("[redacted]"));
    assert!(formatted.contains("expires"));
}

#[test]
fn invalid_json_is_rejected() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
    fs::write(&path, "not json").expect("seed");
    let error = match AuthStorage::new(&path) {
        Ok(_) => panic!("invalid json unexpectedly loaded"),
        Err(error) => error,
    };
    assert!(matches!(error, AuthError::Invalid(_)));
}
#[test]
fn non_regular_auth_path_is_rejected_on_read() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    fs::create_dir_all(&path).expect("create auth path as directory");
    let error = match AuthStorage::new(&path) {
        Ok(_) => panic!("directory auth path unexpectedly loaded"),
        Err(error) => error,
    };
    assert!(matches!(error, AuthError::Invalid(_)));
}
#[test]
fn oversized_auth_file_is_rejected_on_read() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
    fs::write(&path, vec![b' '; (MAX_AUTH_FILE_BYTES + 1) as usize]).expect("seed");
    let error = match AuthStorage::new(&path) {
        Ok(_) => panic!("oversized auth file unexpectedly loaded"),
        Err(error) => error,
    };
    assert!(matches!(error, AuthError::Invalid(_)));
}
#[cfg(unix)]
#[test]
fn symlink_to_regular_file_is_allowed_on_read() {
    use std::os::unix::fs::symlink;
    let temp = TempDir::new().expect("temp dir");
    let real_path = temp.path().join("real-auth.json");
    let link_path = auth_path(&temp);
    fs::create_dir_all(link_path.parent().expect("parent")).expect("create dir");
    fs::write(
        &real_path,
        r#"{"version":1,"providers":{"openrouter":{"type":"api_key","key":"via-link"}}}"#,
    )
    .expect("seed");
    symlink(&real_path, &link_path).expect("symlink");
    let storage = AuthStorage::new(&link_path).expect("load through symlink");
    assert_eq!(storage.get("openrouter"), Some(api_key("via-link")));
}
#[cfg(unix)]
#[test]
fn symlink_target_is_rejected_on_write() {
    use std::os::unix::fs::symlink;
    let temp = TempDir::new().expect("temp dir");
    let real_path = temp.path().join("real-auth.json");
    let link_path = auth_path(&temp);
    fs::create_dir_all(link_path.parent().expect("parent")).expect("create dir");
    fs::write(&real_path, r#"{"version":1,"providers":{}}"#).expect("seed");
    symlink(&real_path, &link_path).expect("symlink");
    let mut storage = AuthStorage::new(&link_path).expect("load through symlink is allowed");
    let error = storage
        .set("openrouter", api_key("k"))
        .expect_err("write through symlink");
    assert!(matches!(error, AuthError::Invalid(_)));
}
#[test]
fn unknown_fields_do_not_cross_credential_type_changes() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
    fs::write(
        &path,
        r#"{
  "version": 1,
  "providers": {
    "chatgpt": {
      "type": "oauth",
      "access": "old-access",
      "refresh": "old-refresh",
      "expires": 1,
      "oauth_future": "drop-when-type-changes"
    }
  }
}"#,
    )
    .expect("seed");
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage.set("chatgpt", api_key("new-key")).expect("set");
    let json = read_json(&path);
    assert!(json["providers"]["chatgpt"].get("oauth_future").is_none());
    assert_eq!(json["providers"]["chatgpt"]["key"], Value::from("new-key"));
}
#[test]
fn remove_persists_and_list_reflects_current_providers() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage.set("openrouter", api_key("one")).expect("set one");
    storage.set("anthropic", api_key("two")).expect("set two");
    storage.remove("openrouter").expect("remove");
    assert_eq!(storage.list(), vec!["anthropic".to_owned()]);
    let loaded = AuthStorage::new(&path).expect("reload");
    assert_eq!(loaded.list(), vec!["anthropic".to_owned()]);
    assert_eq!(loaded.get("openrouter"), None);
}
#[test]
fn empty_provider_id_is_rejected() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    assert!(matches!(
        storage.set("", api_key("key")),
        Err(AuthError::Invalid(_))
    ));
    assert!(matches!(storage.remove(""), Err(AuthError::Invalid(_))));
}
#[test]
fn oauth_status_states_are_reported_without_secret_values() {
    let valid = oauth("valid-access", "valid-refresh", u64::MAX);
    let refreshable = oauth("old-access", "refresh-token", 1);
    let unrefreshable = oauth("old-access", "", 1);
    assert_eq!(state_for_credential(&valid), AuthState::Valid);
    assert_eq!(
        state_for_credential(&refreshable),
        AuthState::ExpiredRefreshable
    );
    assert_eq!(
        state_for_credential(&unrefreshable),
        AuthState::ExpiredUnrefreshable
    );
    assert!(!format!("{valid:?}").contains("valid-access"));
    assert!(!format!("{refreshable:?}").contains("refresh-token"));
}
#[test]
fn stale_temp_files_do_not_block_writes() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    // Leave a stale temp file with a colliding suffix. NamedTempFile
    // internally uses random names, so subsequent writes should succeed.
    let stale = temp.path().join("stale.tmp");
    fs::write(&stale, "garbage").expect("seed stale");
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("openrouter", api_key("ok"))
        .expect("write despite stale file");
    assert_eq!(storage.get("openrouter"), Some(api_key("ok")));
}
#[test]
fn injected_persist_sync_failure_keeps_previous_auth_file() {
    use crate::durability::fault::{arm_matching, Op};

    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("openrouter", api_key("sk-first"))
        .expect("first set");
    let before = fs::read_to_string(&path).expect("auth before");

    // The atomic write syncs its temp file before persisting over the auth
    // file; failing that sync must abort before the previous file changes.
    let guard = arm_matching(Op::FileSync, |candidate| {
        candidate
            .extension()
            .is_some_and(|extension| extension == "tmp")
    });
    let error = storage
        .set("chatgpt", api_key("sk-second"))
        .expect_err("injected persist sync failure");
    assert!(matches!(error, AuthError::Io(_)));
    assert!(guard.fired());
    drop(guard);

    assert_eq!(fs::read_to_string(&path).expect("auth after"), before);
    let reloaded = AuthStorage::new(&path).expect("reload");
    assert_eq!(reloaded.get("openrouter"), Some(api_key("sk-first")));
    assert!(!reloaded.contains_provider_entry("chatgpt"));
}
