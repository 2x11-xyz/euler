use super::*;
use euler_core::auth_storage::SecretString;
use euler_provider::chatgpt::ChatGptProvider;
use euler_provider::openai::OpenAiProvider;
use euler_provider::{ModelProvider, ProviderErrorCategory};
use std::cell::Cell;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    fn set_path(key: &'static str, value: &Path) -> Self {
        let previous = env::var_os(key);
        env::set_var(key, value);
        Self { key, previous }
    }

    fn set(key: &'static str, value: &str) -> Self {
        let previous = env::var_os(key);
        env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => env::set_var(self.key, value),
            None => env::remove_var(self.key),
        }
    }
}

fn auth_path(temp: &TempDir) -> PathBuf {
    temp.path().join(".euler").join("auth.json")
}

fn api_key(key: &str) -> Credential {
    Credential::ApiKey {
        key: SecretString::new(key),
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

fn refresh(access: &str, refresh: Option<&str>, expires: u64) -> ChatGptRefreshCredential {
    ChatGptRefreshCredential {
        access: ProviderSecretString::new(access),
        refresh: refresh.map(ProviderSecretString::new),
        expires,
        account_id: Some("acct-refreshed".to_owned()),
    }
}

fn validation_result(auth: &StoredChatGptAuth) -> String {
    validate_provider_auth_with("chatgpt", None, auth, || {
        Err(ProviderError::auth("legacy fallback should not run"))
    })
    .map(|_| "ok".to_owned())
    .unwrap_or_else(|error| error.to_string())
}

fn load_anthropic_api_key(auth: &StoredApiKeyAuth) -> Result<(), ProviderError> {
    auth.load_api_key("anthropic", "ANTHROPIC_API_KEY", "Anthropic")
        .map(|_| ())
}

fn load_openai_api_key(auth: &StoredApiKeyAuth) -> Result<(), ProviderError> {
    auth.load_api_key("openai", "OPENAI_API_KEY", "OpenAI")
        .map(|_| ())
}

#[test]
fn default_api_key_auth_uses_env_when_stored_entry_is_absent() {
    let _lock = crate::TEST_ENV_LOCK.lock().expect("env lock");
    let temp = TempDir::new().expect("temp dir");
    let _home = EnvGuard::set_path("HOME", temp.path());
    let _env = EnvGuard::set("ANTHROPIC_API_KEY", "env-anthropic-secret");
    let auth = StoredApiKeyAuth::new_default();

    load_anthropic_api_key(&auth).expect("env key");

    let formatted = format!("{auth:?}");
    assert!(!formatted.contains("env-anthropic-secret"));
    assert!(!formatted.contains(temp.path().to_string_lossy().as_ref()));
}

#[test]
fn default_api_key_auth_stored_entry_blocks_env_fallback_when_unresolved() {
    let _lock = crate::TEST_ENV_LOCK.lock().expect("env lock");
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("anthropic", api_key("$MISSING_ANTHROPIC_TEST_KEY"))
        .expect("set");
    let _home = EnvGuard::set_path("HOME", temp.path());
    let _env = EnvGuard::set("ANTHROPIC_API_KEY", "fallback-anthropic-secret");
    let auth = StoredApiKeyAuth::new_default();

    let error = load_anthropic_api_key(&auth).expect_err("unresolved stored entry");
    let message = error.to_string();

    assert_eq!(
        error,
        ProviderError::auth("Anthropic API key is missing; set ANTHROPIC_API_KEY")
    );
    assert!(!message.contains("fallback-anthropic-secret"));
    assert!(!message.contains("MISSING_ANTHROPIC_TEST_KEY"));
}

#[test]
fn default_api_key_auth_malformed_stored_entry_blocks_env_fallback() {
    let _lock = crate::TEST_ENV_LOCK.lock().expect("env lock");
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
    fs::write(
        &path,
        r#"{"version":1,"providers":{"anthropic":{"type":"api_key"}}}"#,
    )
    .expect("seed");
    let _home = EnvGuard::set_path("HOME", temp.path());
    let _env = EnvGuard::set("ANTHROPIC_API_KEY", "fallback-anthropic-secret");
    let auth = StoredApiKeyAuth::new_default();

    let error = load_anthropic_api_key(&auth).expect_err("malformed stored entry");
    let message = error.to_string();

    assert_eq!(
        error,
        ProviderError::auth("Anthropic API key is missing; set ANTHROPIC_API_KEY")
    );
    assert!(!message.contains("fallback-anthropic-secret"));
}

#[test]
fn explicit_api_key_auth_file_is_authoritative_and_redacted() {
    let _lock = crate::TEST_ENV_LOCK.lock().expect("env lock");
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("explicit-auth-secret-path.json");
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("anthropic", api_key("explicit-anthropic-secret"))
        .expect("set");
    let _env = EnvGuard::set("ANTHROPIC_API_KEY", "env-anthropic-secret");
    let auth = StoredApiKeyAuth::auth_file(path.clone());

    load_anthropic_api_key(&auth).expect("explicit auth file key");

    let formatted = format!("{auth:?}");
    assert!(formatted.contains("auth-file"));
    assert!(!formatted.contains("explicit-auth-secret-path"));
    assert!(!formatted.contains("explicit-anthropic-secret"));
    assert!(!formatted.contains("env-anthropic-secret"));
}

#[test]
fn explicit_api_key_auth_file_missing_provider_does_not_fall_back_to_env() {
    let _lock = crate::TEST_ENV_LOCK.lock().expect("env lock");
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("explicit-auth.json");
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("openrouter", api_key("openrouter-secret"))
        .expect("set");
    let _env = EnvGuard::set("ANTHROPIC_API_KEY", "env-anthropic-secret");
    let auth = StoredApiKeyAuth::auth_file(path);

    let error = load_anthropic_api_key(&auth).expect_err("missing provider");
    let message = error.to_string();

    assert_eq!(
        error,
        ProviderError::auth(
            "Anthropic API key is missing from the selected auth file; add an api_key credential for anthropic"
        )
    );
    assert!(!message.contains("openrouter-secret"));
    assert!(!message.contains("env-anthropic-secret"));
}

#[test]
fn explicit_api_key_auth_file_malformed_provider_does_not_fall_back_to_env() {
    let _lock = crate::TEST_ENV_LOCK.lock().expect("env lock");
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("explicit-auth.json");
    fs::write(
        &path,
        r#"{"version":1,"providers":{"anthropic":{"type":"api_key"}}}"#,
    )
    .expect("seed");
    let _env = EnvGuard::set("ANTHROPIC_API_KEY", "env-anthropic-secret");
    let auth = StoredApiKeyAuth::auth_file(path);

    let error = load_anthropic_api_key(&auth).expect_err("malformed provider");
    let message = error.to_string();

    assert_eq!(
        error,
        ProviderError::auth(
            "Anthropic API key is missing from the selected auth file; add an api_key credential for anthropic"
        )
    );
    assert!(!message.contains("env-anthropic-secret"));
}

#[test]
fn openai_default_api_key_auth_uses_provider_keyed_stored_entry_before_env() {
    let _lock = crate::TEST_ENV_LOCK.lock().expect("env lock");
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("openai", api_key("stored-openai-secret"))
        .expect("set openai");
    storage
        .set(
            "chatgpt",
            oauth("chatgpt-access-secret", "chatgpt-refresh-secret", u64::MAX),
        )
        .expect("set chatgpt");
    let _home = EnvGuard::set_path("HOME", temp.path());
    let _env = EnvGuard::set("OPENAI_API_KEY", "env-openai-secret");
    let auth = StoredApiKeyAuth::new_default();

    load_openai_api_key(&auth).expect("stored openai key");

    let formatted = format!("{auth:?}");
    assert!(!formatted.contains("stored-openai-secret"));
    assert!(!formatted.contains("chatgpt-access-secret"));
    assert!(!formatted.contains("env-openai-secret"));
}

#[test]
fn openai_missing_stored_key_can_use_env_without_chatgpt_cross_auth() {
    let _lock = crate::TEST_ENV_LOCK.lock().expect("env lock");
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set(
            "chatgpt",
            oauth("chatgpt-access-secret", "chatgpt-refresh-secret", u64::MAX),
        )
        .expect("set chatgpt");
    let _home = EnvGuard::set_path("HOME", temp.path());
    let _env = EnvGuard::set("OPENAI_API_KEY", "env-openai-secret");
    let auth = StoredApiKeyAuth::new_default();

    load_openai_api_key(&auth).expect("env openai key");

    let formatted = format!("{auth:?}");
    assert!(!formatted.contains("chatgpt-access-secret"));
    assert!(!formatted.contains("env-openai-secret"));
}

#[test]
fn openai_explicit_auth_file_is_authoritative_and_provider_scoped() {
    let _lock = crate::TEST_ENV_LOCK.lock().expect("env lock");
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("explicit-auth.json");
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("openrouter", api_key("openrouter-secret"))
        .expect("set openrouter");
    let _env = EnvGuard::set("OPENAI_API_KEY", "env-openai-secret");
    let auth = StoredApiKeyAuth::auth_file(path);

    let error = load_openai_api_key(&auth).expect_err("missing openai key");
    let message = error.to_string();

    assert_eq!(
        error,
        ProviderError::auth(
            "OpenAI API key is missing from the selected auth file; add an api_key credential for openai"
        )
    );
    assert!(!message.contains("openrouter-secret"));
    assert!(!message.contains("env-openai-secret"));
}

#[test]
fn openai_provider_auth_is_not_satisfied_by_chatgpt_credentials() {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("explicit-chatgpt-only-auth.json");
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set(
            "chatgpt",
            oauth("chatgpt-access-secret", "chatgpt-refresh-secret", u64::MAX),
        )
        .expect("set chatgpt");
    let provider = OpenAiProvider::with_api_key_auth(StoredApiKeyAuth::auth_file(path));

    let error = provider
        .validate_auth()
        .expect_err("missing openai api key");
    let message = error.to_string();

    assert_eq!(
        error,
        ProviderError::auth(
            "OpenAI API key is missing from the selected auth file; add an api_key credential for openai"
        )
    );
    assert!(!message.contains("chatgpt-access-secret"));
    assert!(!message.contains("chatgpt-refresh-secret"));
}

#[test]
fn explicit_api_key_auth_file_missing_path_fails_without_path_details() {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("missing-auth-path-with-secret-name.json");
    let auth = StoredApiKeyAuth::auth_file(path);

    let error = load_anthropic_api_key(&auth).expect_err("missing auth file");
    let message = error.to_string();

    assert!(message.contains("Authentication failed for anthropic"));
    assert!(message.contains("selected auth file could not be read"));
    assert!(!message.contains("missing-auth-path-with-secret-name"));
}

#[test]
fn explicit_api_key_auth_file_directory_fails_without_path_details() {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("auth-dir-with-secret-name");
    fs::create_dir(&path).expect("dir");
    let auth = StoredApiKeyAuth::auth_file(path);

    let error = load_anthropic_api_key(&auth).expect_err("directory auth file");
    let message = error.to_string();

    assert!(message.contains("Authentication failed for anthropic"));
    assert!(message.contains("selected auth file is invalid"));
    assert!(!message.contains("auth-dir-with-secret-name"));
}

#[cfg(unix)]
#[test]
fn explicit_api_key_auth_file_permission_denied_fails_without_path_details() {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("unreadable-auth-secret-path.json");
    fs::write(&path, r#"{"version":1,"providers":{}}"#).expect("seed");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod");
    let auth = StoredApiKeyAuth::auth_file(path.clone());

    let error = load_anthropic_api_key(&auth).expect_err("unreadable auth file");
    let message = error.to_string();

    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("restore chmod");
    assert!(message.contains("Authentication failed for anthropic"));
    assert!(message.contains("selected auth file could not be read"));
    assert!(!message.contains("unreadable-auth-secret-path"));
}

#[test]
fn startup_and_resume_chatgpt_validation_use_same_stored_source() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("chatgpt", oauth("fresh-access", "refresh-secret", 200_000))
        .expect("set");
    let auth = StoredChatGptAuth::with_parts(
        path,
        |_refresh| panic!("fresh credential should not refresh"),
        100_000,
    );

    assert_eq!(validation_result(&auth), "ok");
    assert_eq!(validation_result(&auth), "ok");
}

#[test]
fn explicit_chatgpt_auth_file_delegates_to_legacy_provider_validation() {
    let calls = Cell::new(0);
    let auth = StoredChatGptAuth::with_parts(
        PathBuf::from("unused-euler-auth.json"),
        |_refresh| panic!("stored auth should not run"),
        100_000,
    );

    let error = validate_provider_auth_with(
        "chatgpt",
        Some(Path::new("codex-style-auth.json")),
        &auth,
        || {
            calls.set(calls.get() + 1);
            Err(ProviderError::auth("parsed as Codex AuthFile"))
        },
    )
    .expect_err("provider fallback should decide auth-file validity")
    .to_string();

    assert_eq!(calls.get(), 1);
    assert_eq!(error, "parsed as Codex AuthFile");
}

#[test]
fn stored_missing_chatgpt_auth_does_not_fall_back_to_codex_auth_file() {
    let temp = TempDir::new().expect("temp dir");
    let calls = Cell::new(0);
    let auth = StoredChatGptAuth::with_parts(
        auth_path(&temp),
        |_refresh| panic!("missing credential should not refresh"),
        100_000,
    );

    let error = validate_provider_auth_with("chatgpt", None, &auth, || {
        calls.set(calls.get() + 1);
        Ok(())
    })
    .expect_err("missing stored auth");

    assert_eq!(calls.get(), 0);
    assert!(error
        .to_string()
        .contains("Run: euler login --provider chatgpt"));
}

#[test]
fn stored_expired_refreshable_chatgpt_auth_refreshes_and_redacts_response() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set(
            "chatgpt",
            oauth("old-access-secret", "old-refresh-secret", 100_000),
        )
        .expect("set");
    let auth = StoredChatGptAuth::with_parts(
        path.clone(),
        |refresh_token| {
            assert_eq!(refresh_token, "old-refresh-secret");
            Ok(refresh(
                "new-access-secret",
                Some("new-refresh-secret"),
                200_000,
            ))
        },
        100_000,
    );

    assert_eq!(validation_result(&auth), "ok");
    let written = fs::read_to_string(path).expect("auth file");
    assert!(written.contains("new-access-secret"));

    let debug = format!("{:?}", auth.load().expect("stored credential"));
    assert!(!debug.contains("new-access-secret"));
    assert!(!debug.contains("new-refresh-secret"));
}

#[test]
fn stored_expired_unrefreshable_chatgpt_auth_returns_relogin_error() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("chatgpt", oauth("old-access-secret", "", 100_000))
        .expect("set");
    let auth = StoredChatGptAuth::with_parts(
        path,
        |_refresh| panic!("empty refresh token should not call provider"),
        100_000,
    );

    let error = validation_result(&auth);

    assert!(error.contains("Run: euler login --provider chatgpt"));
    assert!(!error.contains("old-access-secret"));
}

#[test]
fn request_provider_and_validation_path_agree_on_stored_auth_source() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set("chatgpt", oauth("fresh-access", "refresh-secret", 200_000))
        .expect("set");
    let auth = StoredChatGptAuth::with_parts(
        path,
        |_refresh| panic!("fresh credential should not refresh"),
        100_000,
    );
    let provider = ChatGptProvider::stored_euler_auth(auth.clone());

    validate_provider_auth_with("chatgpt", None, &auth, || {
        Err(ProviderError::auth("legacy fallback should not run"))
    })
    .expect("validation");
    provider.validate_auth().expect("provider validation");
}

#[test]
fn stored_refresh_failure_does_not_expose_tokens_or_overwrite_file() {
    let temp = TempDir::new().expect("temp dir");
    let path = auth_path(&temp);
    let mut storage = AuthStorage::new(&path).expect("storage");
    storage
        .set(
            "chatgpt",
            oauth("old-access-secret", "old-refresh-secret", 100_000),
        )
        .expect("set");
    let before = fs::read_to_string(&path).expect("before");
    let auth = StoredChatGptAuth::with_parts(
        path.clone(),
        |_refresh| Err(ProviderError::auth("provider echoed old-refresh-secret")),
        100_000,
    );

    let error = validation_result(&auth);

    assert!(error.contains("Run: euler login --provider chatgpt"));
    assert!(!error.contains("old-access-secret"));
    assert!(!error.contains("old-refresh-secret"));
    assert_eq!(fs::read_to_string(path).expect("after"), before);
}

#[test]
fn stored_auth_file_io_failure_is_auth_error_without_io_details() {
    let error = map_stored_chatgpt_auth_error(AuthError::Io(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "/tmp/auth-with-stored-access-secret.json",
    )));
    let message = error.to_string();

    assert_eq!(error.category(), ProviderErrorCategory::Auth);
    assert!(message.contains("stored auth file could not be read"));
    assert!(message.contains("Run: euler login --provider chatgpt"));
    assert!(!message.contains("/tmp/auth-with-stored-access-secret.json"));
    assert!(!message.contains("stored-access-secret"));
}

#[test]
fn non_chatgpt_validation_uses_provider_fallback() {
    let calls = Cell::new(0);
    let auth = StoredChatGptAuth::with_parts(
        PathBuf::from("unused-euler-auth.json"),
        |_refresh| panic!("stored auth should not run"),
        100_000,
    );

    validate_provider_auth_with("anthropic", None, &auth, || {
        calls.set(calls.get() + 1);
        Ok(())
    })
    .expect("provider fallback succeeds");

    assert_eq!(calls.get(), 1);
}
