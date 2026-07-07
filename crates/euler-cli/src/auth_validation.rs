use anyhow::Result;
use euler_core::auth_storage::{
    AuthError, AuthStorage, Credential, RefreshedOAuthCredential,
    SecretString as StoredSecretString,
};
use euler_provider::auth::{ApiKeyAuth, SecretString as ProviderSecretString};
use euler_provider::chatgpt::{
    refresh_chatgpt_oauth, ChatGptRefreshCredential, ChatGptStoredAuth, ChatGptStoredCredential,
};
use euler_provider::ProviderError;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

type RefreshFn =
    dyn Fn(&str) -> Result<ChatGptRefreshCredential, ProviderError> + Send + Sync + 'static;
type ClockFn = dyn Fn() -> u64 + Send + Sync + 'static;

#[derive(Clone)]
pub(crate) struct StoredChatGptAuth {
    storage_path: Option<PathBuf>,
    refresh: Arc<RefreshFn>,
    clock: Arc<ClockFn>,
}

#[derive(Clone)]
pub(crate) struct StoredApiKeyAuth {
    source: ApiKeyAuthSource,
}

#[derive(Clone)]
enum ApiKeyAuthSource {
    Default,
    AuthFile(PathBuf),
}

impl StoredApiKeyAuth {
    pub(crate) fn new_default() -> Self {
        Self {
            source: ApiKeyAuthSource::Default,
        }
    }

    pub(crate) fn auth_file(path: PathBuf) -> Self {
        Self {
            source: ApiKeyAuthSource::AuthFile(path),
        }
    }

    fn open_storage(&self) -> Result<AuthStorage, AuthError> {
        match &self.source {
            ApiKeyAuthSource::Default => AuthStorage::new_default(),
            ApiKeyAuthSource::AuthFile(path) => {
                fs::metadata(path).map_err(AuthError::Io)?;
                AuthStorage::new_auth_file(path)
            }
        }
    }

    fn uses_explicit_auth_file(&self) -> bool {
        matches!(self.source, ApiKeyAuthSource::AuthFile(_))
    }
}

impl fmt::Debug for StoredApiKeyAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let source = match &self.source {
            ApiKeyAuthSource::Default => "default",
            ApiKeyAuthSource::AuthFile(_) => "auth-file",
        };
        f.debug_struct("StoredApiKeyAuth")
            .field("source", &source)
            .finish()
    }
}

impl ApiKeyAuth for StoredApiKeyAuth {
    fn load_api_key(
        &self,
        provider_id: &'static str,
        env_key_name: &'static str,
        display_name: &'static str,
    ) -> Result<ProviderSecretString, ProviderError> {
        let storage = self.open_storage().map_err(|error| {
            map_stored_api_key_auth_error(
                provider_id,
                env_key_name,
                self.uses_explicit_auth_file(),
                error,
            )
        })?;
        let stored_entry_exists = storage.contains_provider_entry(provider_id);
        let env_key = if self.uses_explicit_auth_file() || stored_entry_exists {
            None
        } else {
            Some(env_key_name)
        };
        let Some(key) = storage.resolve_api_key(provider_id, env_key) else {
            return Err(api_key_missing_error(
                provider_id,
                display_name,
                env_key_name,
                self.uses_explicit_auth_file(),
            ));
        };
        Ok(ProviderSecretString::new(key.expose_secret().to_owned()))
    }
}

impl StoredChatGptAuth {
    pub(crate) fn new_default() -> Self {
        Self {
            storage_path: None,
            refresh: Arc::new(refresh_chatgpt_oauth),
            clock: Arc::new(now_unix_ms),
        }
    }

    #[cfg(test)]
    fn with_parts(
        storage_path: PathBuf,
        refresh: impl Fn(&str) -> Result<ChatGptRefreshCredential, ProviderError>
            + Send
            + Sync
            + 'static,
        now_ms: u64,
    ) -> Self {
        Self {
            storage_path: Some(storage_path),
            refresh: Arc::new(refresh),
            clock: Arc::new(move || now_ms),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ProviderError> {
        self.load().map(|_| ())
    }

    fn open_storage(&self) -> Result<AuthStorage, AuthError> {
        match &self.storage_path {
            Some(path) => AuthStorage::new(path),
            None => AuthStorage::new_default(),
        }
    }
}

impl ChatGptStoredAuth for StoredChatGptAuth {
    fn load(&self) -> Result<ChatGptStoredCredential, ProviderError> {
        let mut storage = self.open_storage().map_err(map_stored_chatgpt_auth_error)?;
        let credential = storage
            .refresh_oauth_if_needed("chatgpt", (self.clock)(), |refresh| {
                let refreshed = (self.refresh)(refresh.expose_secret())
                    .map_err(|_| AuthError::RefreshFailed)?;
                Ok(refreshed_oauth(refreshed))
            })
            .map_err(map_stored_chatgpt_auth_error)?;

        stored_credential_from_core(credential)
    }
}

pub(crate) fn validate_provider_auth(
    provider_id: &str,
    auth_file: Option<&Path>,
    fallback: impl FnOnce() -> Result<(), ProviderError>,
) -> Result<()> {
    validate_provider_auth_with(
        provider_id,
        auth_file,
        &StoredChatGptAuth::new_default(),
        fallback,
    )
}

fn validate_provider_auth_with(
    provider_id: &str,
    auth_file: Option<&Path>,
    stored_chatgpt_auth: &StoredChatGptAuth,
    fallback: impl FnOnce() -> Result<(), ProviderError>,
) -> Result<()> {
    if provider_id == "chatgpt" && auth_file.is_none() {
        return stored_chatgpt_auth.validate().map_err(Into::into);
    }

    fallback().map_err(Into::into)
}

fn stored_credential_from_core(
    credential: Credential,
) -> Result<ChatGptStoredCredential, ProviderError> {
    let Credential::OAuth {
        access, account_id, ..
    } = credential
    else {
        return Err(chatgpt_auth_error(
            "stored credential is not an OAuth credential",
        ));
    };
    if access.expose_secret().is_empty() {
        return Err(chatgpt_auth_error("stored OAuth access token is missing"));
    }
    let Some(account_id) = account_id.filter(|value| !value.is_empty()) else {
        return Err(chatgpt_auth_error("stored ChatGPT account id is missing"));
    };

    Ok(ChatGptStoredCredential {
        access_token: ProviderSecretString::new(access.expose_secret().to_owned()),
        account_id,
    })
}

fn refreshed_oauth(refreshed: ChatGptRefreshCredential) -> RefreshedOAuthCredential {
    let parts = refreshed.into_storage_parts();
    RefreshedOAuthCredential {
        access: StoredSecretString::new(parts.access),
        refresh: parts.refresh.map(StoredSecretString::new),
        expires: parts.expires,
        account_id: parts.account_id,
    }
}

fn map_stored_chatgpt_auth_error(error: AuthError) -> ProviderError {
    match error {
        AuthError::Missing => chatgpt_auth_error("no stored ChatGPT credential found"),
        AuthError::Expired | AuthError::RefreshFailed => {
            chatgpt_auth_error("stored token expired and could not be refreshed")
        }
        AuthError::Invalid(_) => chatgpt_auth_error("stored credential is invalid"),
        AuthError::Io(_) => chatgpt_auth_error("stored auth file could not be read"),
    }
}

fn map_stored_api_key_auth_error(
    provider_id: &str,
    env_key_name: &str,
    explicit_auth_file: bool,
    error: AuthError,
) -> ProviderError {
    if explicit_auth_file {
        let problem = match error {
            AuthError::Invalid(_) => "is invalid",
            AuthError::Io(_) => "could not be read",
            AuthError::Missing => "is missing a credential",
            AuthError::Expired => "contains an expired credential",
            AuthError::RefreshFailed => "could not refresh a credential",
        };
        return ProviderError::auth(format!(
            "Authentication failed for {provider_id}: selected auth file {problem}.\nAdd an api_key credential for {provider_id} to the selected auth file"
        ));
    }
    let problem = match error {
        AuthError::Invalid(_) => "auth storage is invalid",
        AuthError::Io(_) => "auth storage could not be read",
        AuthError::Missing => "credential is missing",
        AuthError::Expired => "credential is expired",
        AuthError::RefreshFailed => "credential refresh failed",
    };
    ProviderError::auth(format!(
        "Authentication failed for {provider_id}: {problem}.\nSet {env_key_name} or add an api_key credential to ~/.euler/auth.json"
    ))
}

fn api_key_missing_error(
    provider_id: &'static str,
    display_name: &'static str,
    env_key_name: &'static str,
    explicit_auth_file: bool,
) -> ProviderError {
    if explicit_auth_file {
        return ProviderError::auth(format!(
            "{display_name} API key is missing from the selected auth file; add an api_key credential for {provider_id}"
        ));
    }
    ProviderError::auth(format!(
        "{display_name} API key is missing; set {env_key_name}"
    ))
}

fn chatgpt_auth_error(problem: &str) -> ProviderError {
    ProviderError::auth(format!(
        "Authentication failed for chatgpt: {problem}.\nRun: euler login --provider chatgpt"
    ))
}

fn now_unix_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    millis.min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
#[path = "auth_validation_test.rs"]
mod tests;
