use serde::Deserialize;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::ProviderError;

const DEFAULT_CODEX_AUTH_PATH: &str = ".codex/auth.json";

#[derive(Clone, Eq, PartialEq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn into_string(self) -> String {
        self.0
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

pub trait ApiKeyAuth: fmt::Debug + Send + Sync {
    fn load_api_key(
        &self,
        provider_id: &'static str,
        env_key_name: &'static str,
        display_name: &'static str,
    ) -> Result<SecretString, ProviderError>;
}

#[derive(Clone, Debug, Default)]
pub struct EnvApiKeyAuth;

impl ApiKeyAuth for EnvApiKeyAuth {
    fn load_api_key(
        &self,
        _provider_id: &'static str,
        env_key_name: &'static str,
        display_name: &'static str,
    ) -> Result<SecretString, ProviderError> {
        api_key_from_env_value(display_name, env_key_name, std::env::var_os(env_key_name))
    }
}

pub(crate) fn api_key_from_env_value(
    display_name: &'static str,
    env_key_name: &'static str,
    value: Option<OsString>,
) -> Result<SecretString, ProviderError> {
    let Some(value) = value else {
        return Err(missing_api_key_error(display_name, env_key_name));
    };
    let value = value.to_string_lossy().trim().to_owned();
    if value.is_empty() {
        return Err(missing_api_key_error(display_name, env_key_name));
    }
    Ok(SecretString::new(value))
}

pub(crate) fn missing_api_key_error(
    display_name: &'static str,
    env_key_name: &'static str,
) -> ProviderError {
    ProviderError::auth(format!(
        "{display_name} API key is missing; set {env_key_name}"
    ))
}

#[derive(Clone, Eq, PartialEq)]
pub struct ChatGptCredentials {
    pub id_token: SecretString,
    pub access_token: SecretString,
    pub refresh_token: SecretString,
    pub account_id: SecretString,
}

impl fmt::Debug for ChatGptCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatGptCredentials")
            .field("id_token", &self.id_token)
            .field("access_token", &self.access_token)
            .field("refresh_token", &self.refresh_token)
            .field("account_id", &self.account_id)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthFile {
    path: AuthPath,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AuthPath {
    Path(PathBuf),
    MissingHome,
}

impl AuthFile {
    pub fn default_codex() -> Self {
        Self {
            path: default_codex_auth_path(std::env::var_os("HOME")),
        }
    }

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: AuthPath::Path(path.into()),
        }
    }

    pub fn load(&self) -> Result<ChatGptCredentials, ProviderError> {
        let path = self.path.resolved()?;
        let content = fs::read_to_string(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ProviderError::auth(format!(
                    "ChatGPT auth file not found at {}; refresh via Codex or pass --auth-file",
                    path.display()
                ))
            } else {
                ProviderError::transport(format!(
                    "failed to read ChatGPT auth file at {}: {error}",
                    path.display()
                ))
            }
        })?;
        let parsed: CodexAuthFile = serde_json::from_str(&content).map_err(|error| {
            ProviderError::auth(format!(
                "failed to parse ChatGPT auth file at {}: {error}",
                path.display()
            ))
        })?;
        parsed.into_credentials()
    }
}

impl AuthPath {
    fn resolved(&self) -> Result<&Path, ProviderError> {
        match self {
            Self::Path(path) => Ok(path),
            Self::MissingHome => Err(ProviderError::auth(
                "cannot locate default ChatGPT auth file because HOME is unset; pass --auth-file",
            )),
        }
    }
}

fn default_codex_auth_path(home: Option<OsString>) -> AuthPath {
    home.map(PathBuf::from)
        .map(|home| AuthPath::Path(home.join(Path::new(DEFAULT_CODEX_AUTH_PATH))))
        .unwrap_or(AuthPath::MissingHome)
}

#[derive(Deserialize)]
struct CodexAuthFile {
    tokens: CodexAuthTokens,
}

#[derive(Deserialize)]
struct CodexAuthTokens {
    id_token: String,
    access_token: String,
    refresh_token: String,
    account_id: String,
}

impl CodexAuthFile {
    fn into_credentials(self) -> Result<ChatGptCredentials, ProviderError> {
        if self.tokens.access_token.is_empty() || self.tokens.account_id.is_empty() {
            return Err(ProviderError::auth(
                "ChatGPT auth file is missing required token fields",
            ));
        }
        Ok(ChatGptCredentials {
            id_token: SecretString::new(self.tokens.id_token),
            access_token: SecretString::new(self.tokens.access_token),
            refresh_token: SecretString::new(self.tokens.refresh_token),
            account_id: SecretString::new(self.tokens.account_id),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_debug_redacts_token_values() {
        let credentials = ChatGptCredentials {
            id_token: SecretString::new("id-secret"),
            access_token: SecretString::new("access-secret"),
            refresh_token: SecretString::new("refresh-secret"),
            account_id: SecretString::new("account-secret"),
        };

        let formatted = format!("{credentials:?}");

        assert!(formatted.contains("[redacted]"));
        assert!(!formatted.contains("id-secret"));
        assert!(!formatted.contains("access-secret"));
        assert!(!formatted.contains("refresh-secret"));
        assert!(!formatted.contains("account-secret"));
    }

    #[test]
    fn default_path_requires_home() {
        let auth_file = AuthFile {
            path: default_codex_auth_path(None),
        };

        let error = auth_file.load().expect_err("missing HOME");

        assert_eq!(
            error,
            ProviderError::auth(
                "cannot locate default ChatGPT auth file because HOME is unset; pass --auth-file"
            )
        );
    }

    #[test]
    fn default_path_uses_home_when_available() {
        assert_eq!(
            default_codex_auth_path(Some(OsString::from("/home/alice"))),
            AuthPath::Path(PathBuf::from("/home/alice/.codex/auth.json"))
        );
    }
}
