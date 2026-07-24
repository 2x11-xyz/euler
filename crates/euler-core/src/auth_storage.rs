use crate::durability::{sync_dir, sync_file_all};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;
use thiserror::Error;

const CURRENT_VERSION: u64 = 1;
const DEFAULT_AUTH_PATH: &str = ".euler/auth.json";
const TEMP_SUFFIX: &str = ".tmp";
const MAX_AUTH_FILE_BYTES: u64 = 1024 * 1024;
const OAUTH_REFRESH_MARGIN_MS: u64 = 30_000;

type EnvLookup = dyn Fn(&str) -> Option<String> + Send + Sync;

#[derive(Clone, Eq, PartialEq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
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

#[derive(Clone, Eq, PartialEq)]
pub enum Credential {
    ApiKey {
        key: SecretString,
    },
    OAuth {
        access: SecretString,
        refresh: SecretString,
        expires: u64,
        account_id: Option<String>,
    },
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Credential::ApiKey { .. } => f
                .debug_struct("ApiKey")
                .field("key", &"[redacted]")
                .finish(),
            Credential::OAuth {
                expires,
                account_id,
                ..
            } => f
                .debug_struct("OAuth")
                .field("access", &"[redacted]")
                .field("refresh", &"[redacted]")
                .field("expires", expires)
                .field("account_id", account_id)
                .finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthStatus {
    pub configured: bool,
    pub source: AuthSource,
    pub state: AuthState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthSource {
    Runtime,
    AuthFile,
    Stored,
    Env,
    Missing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthState {
    Valid,
    ExpiredRefreshable,
    ExpiredUnrefreshable,
    Missing,
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("auth credential is missing")]
    Missing,
    #[error("invalid auth storage: {0}")]
    Invalid(String),
    #[error("auth credential is expired")]
    Expired,
    #[error("oauth refresh failed")]
    RefreshFailed,
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Clone, Eq, PartialEq)]
pub struct RefreshedOAuthCredential {
    pub access: SecretString,
    pub refresh: Option<SecretString>,
    pub expires: u64,
    pub account_id: Option<String>,
}

impl fmt::Debug for RefreshedOAuthCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefreshedOAuthCredential")
            .field("access", &self.access)
            .field("refresh", &self.refresh)
            .field("expires", &self.expires)
            .field("account_id", &self.account_id)
            .finish()
    }
}

pub struct AuthStorage {
    path: PathBuf,
    document: Value,
    storage_source: AuthSource,
    runtime_api_keys: BTreeMap<String, SecretString>,
    env_lookup: Arc<EnvLookup>,
}

impl AuthStorage {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, AuthError> {
        Self::with_env_lookup(path, |name| env::var(name).ok())
    }

    pub fn new_auth_file(path: impl AsRef<Path>) -> Result<Self, AuthError> {
        Self::with_env_lookup_and_source(path, AuthSource::AuthFile, |name| env::var(name).ok())
    }

    pub fn new_default() -> Result<Self, AuthError> {
        Self::new(Self::default_path()?)
    }

    pub fn default_path() -> Result<PathBuf, AuthError> {
        let home = env::var_os("HOME").ok_or_else(|| {
            AuthError::Invalid("cannot locate default auth file because HOME is unset".to_owned())
        })?;
        Ok(PathBuf::from(home).join(DEFAULT_AUTH_PATH))
    }

    fn with_env_lookup(
        path: impl AsRef<Path>,
        env_lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Result<Self, AuthError> {
        Self::with_env_lookup_and_source(path, AuthSource::Stored, env_lookup)
    }

    fn with_env_lookup_and_source(
        path: impl AsRef<Path>,
        storage_source: AuthSource,
        env_lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Result<Self, AuthError> {
        let path = path.as_ref().to_path_buf();
        let document = load_document(&path)?;
        warn_if_forward_version(&document);
        Ok(Self {
            path,
            document,
            storage_source,
            runtime_api_keys: BTreeMap::new(),
            env_lookup: Arc::new(env_lookup),
        })
    }

    pub fn get(&self, provider: &str) -> Option<Credential> {
        provider_value(&self.document, provider).and_then(credential_from_value)
    }

    pub fn set(&mut self, provider: &str, credential: Credential) -> Result<(), AuthError> {
        validate_provider(provider)?;
        self.mutate(|document| {
            let providers = providers_object_mut(document)?;
            let existing = providers.get(provider);
            providers.insert(
                provider.to_owned(),
                credential_to_value(&credential, existing),
            );
            Ok(())
        })
    }

    pub fn remove(&mut self, provider: &str) -> Result<(), AuthError> {
        validate_provider(provider)?;
        self.mutate(|document| {
            providers_object_mut(document)?.remove(provider);
            Ok(())
        })
    }

    pub fn list(&self) -> Vec<String> {
        providers_object(&self.document)
            .map(|providers| providers.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn has(&self, provider: &str) -> bool {
        self.get(provider).is_some()
    }

    /// Reports whether the provider has any stored entry, even if that entry
    /// is malformed or unresolved. API-key auth uses this to make a stored
    /// provider entry authoritative instead of silently falling back to env.
    pub fn contains_provider_entry(&self, provider: &str) -> bool {
        provider_value(&self.document, provider).is_some()
    }

    pub fn set_runtime_api_key(&mut self, provider: &str, key: String) {
        self.runtime_api_keys
            .insert(provider.to_owned(), SecretString::new(key));
    }

    pub fn remove_runtime_api_key(&mut self, provider: &str) {
        self.runtime_api_keys.remove(provider);
    }

    pub fn auth_status(&self, provider: &str, env_key_name: Option<&str>) -> AuthStatus {
        if self.runtime_api_keys.contains_key(provider) {
            return AuthStatus {
                configured: true,
                source: AuthSource::Runtime,
                state: AuthState::Valid,
            };
        }

        if let Some(credential) = self.get(provider) {
            return AuthStatus {
                configured: true,
                source: self.storage_source,
                state: state_for_credential(&credential),
            };
        }

        if self.env_api_key(env_key_name).is_some() {
            return AuthStatus {
                configured: true,
                source: AuthSource::Env,
                state: AuthState::Valid,
            };
        }

        AuthStatus {
            configured: false,
            source: AuthSource::Missing,
            state: AuthState::Missing,
        }
    }

    pub fn resolve_api_key(
        &self,
        provider: &str,
        env_key_name: Option<&str>,
    ) -> Option<SecretString> {
        if let Some(key) = self.runtime_api_keys.get(provider) {
            return Some(key.clone());
        }

        if let Some(Credential::ApiKey { key }) = self.get(provider) {
            if let Some(resolved) = resolve_config_value(&key, self.env_lookup.as_ref()) {
                return Some(resolved);
            }
        }

        self.env_api_key(env_key_name)
    }

    pub fn refresh_oauth_if_needed<F>(
        &mut self,
        provider: &str,
        now_ms: u64,
        refresh: F,
    ) -> Result<Credential, AuthError>
    where
        F: FnOnce(&SecretString) -> Result<RefreshedOAuthCredential, AuthError>,
    {
        validate_provider(provider)?;
        let credential = self.get(provider).ok_or(AuthError::Missing)?;
        let Credential::OAuth { expires, .. } = credential else {
            return Err(AuthError::Invalid(
                "stored credential is not an OAuth credential".to_owned(),
            ));
        };
        if oauth_is_fresh(now_ms, expires) {
            return Ok(credential);
        }
        self.ensure_oauth_refresh_allowed()?;

        let _lock = acquire_lock(&self.path)?;
        reject_symlink_for_write(&self.path)?;

        let mut stored = match self.oauth_refresh_readiness(provider, now_ms)? {
            OAuthRefreshReadiness::Fresh { document, existing } => {
                self.document = document;
                return credential_from_value(&existing).ok_or_else(malformed_credential_error);
            }
            OAuthRefreshReadiness::Expired { document } => {
                self.document = document;
                return Err(AuthError::Expired);
            }
            OAuthRefreshReadiness::NeedsRefresh(stored) => stored,
        };

        let refreshed = match refresh(&stored.old_refresh) {
            Ok(refreshed) => refreshed,
            Err(_) => {
                self.document = stored.document;
                return Err(AuthError::RefreshFailed);
            }
        };
        if refreshed.access.is_empty() {
            self.document = stored.document;
            return Err(AuthError::Invalid(
                "OAuth refresh response was missing an access token".to_owned(),
            ));
        }
        if !oauth_is_fresh(now_ms, refreshed.expires) {
            self.document = stored.document;
            return Err(AuthError::RefreshFailed);
        }
        let new_credential = Credential::OAuth {
            access: refreshed.access,
            refresh: refreshed.refresh.unwrap_or(stored.old_refresh),
            expires: refreshed.expires,
            account_id: refreshed.account_id.or(stored.old_account_id),
        };
        providers_object_mut(&mut stored.document)?.insert(
            provider.to_owned(),
            credential_to_value(&new_credential, Some(&stored.existing)),
        );
        write_document_atomic(&self.path, &stored.document)?;
        self.document = stored.document;
        Ok(new_credential)
    }

    fn ensure_oauth_refresh_allowed(&self) -> Result<(), AuthError> {
        if self.storage_source == AuthSource::AuthFile {
            return Err(AuthError::Invalid(
                "explicit auth-file storage is read-only; OAuth credentials cannot be refreshed"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    fn oauth_refresh_readiness(
        &self,
        provider: &str,
        now_ms: u64,
    ) -> Result<OAuthRefreshReadiness, AuthError> {
        let document = load_mutable_document(&self.path)?;
        let existing = provider_value(&document, provider)
            .ok_or(AuthError::Missing)?
            .clone();
        let oauth = oauth_refresh_parts(&existing)?;
        if oauth_is_fresh(now_ms, oauth.expires) {
            return Ok(OAuthRefreshReadiness::Fresh { document, existing });
        }
        if oauth.old_refresh.is_empty() {
            return Ok(OAuthRefreshReadiness::Expired { document });
        }
        Ok(OAuthRefreshReadiness::NeedsRefresh(StoredOAuthRefresh {
            document,
            existing,
            old_refresh: oauth.old_refresh,
            old_account_id: oauth.old_account_id,
        }))
    }

    fn env_api_key(&self, env_key_name: Option<&str>) -> Option<SecretString> {
        let name = env_key_name?;
        (self.env_lookup)(name)
            .filter(|value| !value.is_empty())
            .map(SecretString::new)
    }

    fn mutate(
        &mut self,
        f: impl FnOnce(&mut Value) -> Result<(), AuthError>,
    ) -> Result<(), AuthError> {
        if self.storage_source == AuthSource::AuthFile {
            return Err(AuthError::Invalid(
                "explicit auth-file storage is read-only".to_owned(),
            ));
        }
        let _lock = acquire_lock(&self.path)?;
        reject_symlink_for_write(&self.path)?;

        let mut document = load_mutable_document(&self.path)?;

        f(&mut document)?;
        write_document_atomic(&self.path, &document)?;
        self.document = document;
        Ok(())
    }
}

struct StoredOAuthRefresh {
    document: Value,
    existing: Value,
    old_refresh: SecretString,
    old_account_id: Option<String>,
}

enum OAuthRefreshReadiness {
    Fresh { document: Value, existing: Value },
    Expired { document: Value },
    NeedsRefresh(StoredOAuthRefresh),
}

struct OAuthRefreshParts {
    old_refresh: SecretString,
    expires: u64,
    old_account_id: Option<String>,
}

fn load_mutable_document(path: &Path) -> Result<Value, AuthError> {
    let document = load_document(path)?;
    if let Some(version) = forward_version(&document) {
        warn_forward_version(version);
        return Err(AuthError::Invalid(format!(
            "auth file version {version} is newer than supported version {CURRENT_VERSION}; refusing to mutate"
        )));
    }
    Ok(document)
}

fn oauth_refresh_parts(existing: &Value) -> Result<OAuthRefreshParts, AuthError> {
    let Credential::OAuth {
        refresh: old_refresh,
        expires,
        account_id: old_account_id,
        ..
    } = credential_from_value(existing).ok_or_else(malformed_credential_error)?
    else {
        return Err(AuthError::Invalid(
            "stored credential is not an OAuth credential".to_owned(),
        ));
    };
    Ok(OAuthRefreshParts {
        old_refresh,
        expires,
        old_account_id,
    })
}

fn malformed_credential_error() -> AuthError {
    AuthError::Invalid("stored credential is malformed or unsupported".to_owned())
}

fn resolve_config_value(value: &SecretString, env_lookup: &EnvLookup) -> Option<SecretString> {
    let raw = value.expose_secret();
    if let Some(literal) = raw.strip_prefix("$$") {
        return Some(SecretString::new(format!("${literal}")));
    }
    if let Some(literal) = raw.strip_prefix("$!") {
        return Some(SecretString::new(format!("!{literal}")));
    }
    if raw.starts_with('!') {
        return None;
    }
    if let Some(name) = raw
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
    {
        return resolve_env_name(name, env_lookup);
    }
    if raw.starts_with("${") {
        let expanded_name = expand_braced_env_name(raw, env_lookup)?;
        return resolve_env_name(&expanded_name, env_lookup);
    }
    if let Some(name) = raw.strip_prefix('$') {
        return resolve_env_name(name, env_lookup);
    }
    Some(value.clone())
}

fn resolve_env_name(name: &str, env_lookup: &EnvLookup) -> Option<SecretString> {
    env_lookup(name)
        .filter(|value| !value.is_empty())
        .map(SecretString::new)
}

fn expand_braced_env_name(raw: &str, env_lookup: &EnvLookup) -> Option<String> {
    let mut expanded = String::new();
    let mut rest = raw;

    while let Some(start) = rest.find("${") {
        expanded.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let end = after_open.find('}')?;
        let name = &after_open[..end];
        if name.is_empty() {
            return None;
        }
        expanded.push_str(env_lookup(name).filter(|value| !value.is_empty())?.as_str());
        rest = &after_open[end + 1..];
    }

    expanded.push_str(rest);
    Some(expanded)
}

fn validate_provider(provider: &str) -> Result<(), AuthError> {
    if provider.is_empty() {
        return Err(AuthError::Invalid("provider id is empty".to_owned()));
    }
    Ok(())
}

fn empty_document() -> Value {
    let mut root = Map::new();
    root.insert("version".to_owned(), Value::from(CURRENT_VERSION));
    root.insert("providers".to_owned(), Value::Object(Map::new()));
    Value::Object(root)
}

fn load_document(path: &Path) -> Result<Value, AuthError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(empty_document()),
        Err(error) => return Err(AuthError::Io(error)),
    };
    let metadata = file.metadata()?;
    validate_file_for_read(path, &metadata)?;
    if metadata.len() > MAX_AUTH_FILE_BYTES {
        return Err(AuthError::Invalid(format!(
            "auth file is too large: {} bytes (max {MAX_AUTH_FILE_BYTES})",
            metadata.len()
        )));
    }
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    let document: Value = serde_json::from_str(&content)
        .map_err(|error| AuthError::Invalid(format!("failed to parse auth file: {error}")))?;
    validate_document(&document)?;
    Ok(document)
}

fn validate_file_for_read(path: &Path, metadata: &fs::Metadata) -> Result<(), AuthError> {
    if !metadata.is_file() {
        return Err(AuthError::Invalid(format!(
            "auth path is not a regular file: {}",
            path.display()
        )));
    }
    validate_owner(path, metadata)?;
    warn_if_insecure_file_permissions(path, metadata);
    Ok(())
}

fn validate_document(document: &Value) -> Result<(), AuthError> {
    let root = document
        .as_object()
        .ok_or_else(|| AuthError::Invalid("auth file root must be an object".to_owned()))?;
    let version = root
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| AuthError::Invalid("auth file version must be an integer".to_owned()))?;
    if version == 0 {
        return Err(AuthError::Invalid(
            "auth file version must be at least 1".to_owned(),
        ));
    }
    root.get("providers")
        .and_then(Value::as_object)
        .ok_or_else(|| AuthError::Invalid("auth file providers must be an object".to_owned()))?;
    Ok(())
}

fn providers_object(document: &Value) -> Option<&Map<String, Value>> {
    document.get("providers")?.as_object()
}

fn providers_object_mut(document: &mut Value) -> Result<&mut Map<String, Value>, AuthError> {
    document
        .get_mut("providers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| AuthError::Invalid("auth file providers must be an object".to_owned()))
}

fn provider_value<'a>(document: &'a Value, provider: &str) -> Option<&'a Value> {
    providers_object(document)?.get(provider)
}

fn credential_from_value(value: &Value) -> Option<Credential> {
    let object = value.as_object()?;
    match object.get("type")?.as_str()? {
        "api_key" => Some(Credential::ApiKey {
            key: SecretString::new(object.get("key")?.as_str()?.to_owned()),
        }),
        "oauth" => Some(Credential::OAuth {
            access: SecretString::new(object.get("access")?.as_str()?.to_owned()),
            refresh: SecretString::new(object.get("refresh")?.as_str()?.to_owned()),
            expires: object.get("expires")?.as_u64()?,
            account_id: object
                .get("account_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        _ => None,
    }
}

fn credential_to_value(credential: &Credential, existing: Option<&Value>) -> Value {
    let new_type = match credential {
        Credential::ApiKey { .. } => "api_key",
        Credential::OAuth { .. } => "oauth",
    };
    let existing_same_type = existing
        .and_then(Value::as_object)
        .and_then(|object| object.get("type"))
        .and_then(Value::as_str)
        == Some(new_type);
    let mut object = if existing_same_type {
        existing
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    } else {
        Map::new()
    };

    for known in ["type", "key", "access", "refresh", "expires", "account_id"] {
        object.remove(known);
    }

    match credential {
        Credential::ApiKey { key } => {
            object.insert("type".to_owned(), Value::from("api_key"));
            object.insert("key".to_owned(), Value::from(key.expose_secret()));
        }
        Credential::OAuth {
            access,
            refresh,
            expires,
            account_id,
        } => {
            object.insert("type".to_owned(), Value::from("oauth"));
            object.insert("access".to_owned(), Value::from(access.expose_secret()));
            object.insert("refresh".to_owned(), Value::from(refresh.expose_secret()));
            object.insert("expires".to_owned(), Value::from(*expires));
            if let Some(account_id) = account_id {
                object.insert("account_id".to_owned(), Value::from(account_id.clone()));
            }
        }
    }

    Value::Object(object)
}

fn state_for_credential(credential: &Credential) -> AuthState {
    match credential {
        Credential::ApiKey { .. } => AuthState::Valid,
        Credential::OAuth {
            refresh, expires, ..
        } => {
            if *expires > now_unix_ms() {
                AuthState::Valid
            } else if refresh.is_empty() {
                AuthState::ExpiredUnrefreshable
            } else {
                AuthState::ExpiredRefreshable
            }
        }
    }
}

fn oauth_is_fresh(now_ms: u64, expires: u64) -> bool {
    now_ms.saturating_add(OAUTH_REFRESH_MARGIN_MS) < expires
}

fn now_unix_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    millis.min(u128::from(u64::MAX)) as u64
}

fn forward_version(document: &Value) -> Option<u64> {
    let version = document.get("version")?.as_u64()?;
    (version > CURRENT_VERSION).then_some(version)
}

fn warn_if_forward_version(document: &Value) {
    if let Some(version) = forward_version(document) {
        warn_forward_version(version);
    }
}

fn warn_forward_version(version: u64) {
    eprintln!(
        "warning: auth file version {version} is newer than supported version {CURRENT_VERSION}; mutations are disabled"
    );
}

fn acquire_lock(path: &Path) -> Result<File, AuthError> {
    let dir = containing_dir(path);
    ensure_private_dir(dir)?;
    let lock_path = lock_path_for(path);
    let lock = private_open_options()
        .read(true)
        .write(true)
        .create(true)
        .open(lock_path)?;
    set_file_mode_0600(&lock)?;
    <File as fs4::FileExt>::lock(&lock)?;
    Ok(lock)
}

fn write_document_atomic(path: &Path, document: &Value) -> Result<(), AuthError> {
    let dir = containing_dir(path);
    ensure_private_dir(dir)?;
    let bytes = serde_json::to_vec_pretty(document)
        .map_err(|error| AuthError::Invalid(format!("failed to serialize auth file: {error}")))?;

    let mut temp_file = NamedTempFile::with_suffix_in(TEMP_SUFFIX, dir).map_err(AuthError::Io)?;
    {
        let temp_path = temp_file.path().to_path_buf();
        let file = temp_file.as_file_mut();
        set_file_mode_0600(file)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.flush()?;
        sync_file_all(file, &temp_path)?;
    }

    temp_file
        .persist(path)
        .map_err(|error| AuthError::Io(error.error))?;

    sync_dir(dir)?;
    Ok(())
}

fn reject_symlink_for_write(path: &Path) -> Result<(), AuthError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(AuthError::Invalid(format!(
            "refusing to write auth file through symlink at {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AuthError::Io(error)),
    }
}

fn lock_path_for(path: &Path) -> PathBuf {
    let mut lock_path: OsString = path.as_os_str().to_owned();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}

fn containing_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn ensure_private_dir(path: &Path) -> Result<(), AuthError> {
    fs::create_dir_all(path)?;
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir() {
        return Err(AuthError::Invalid(format!(
            "auth directory path is not a directory: {}",
            path.display()
        )));
    }
    validate_owner(path, &metadata)?;
    set_dir_mode_0700(path)?;
    Ok(())
}

#[cfg(unix)]
fn validate_owner(path: &Path, metadata: &fs::Metadata) -> Result<(), AuthError> {
    use std::os::unix::fs::MetadataExt;

    let owner = metadata.uid();
    let current = current_euid();
    if owner != current {
        return Err(AuthError::Invalid(format!(
            "auth path must be owned by the current user: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_owner(_path: &Path, _metadata: &fs::Metadata) -> Result<(), AuthError> {
    Ok(())
}

#[cfg(unix)]
fn current_euid() -> u32 {
    // SAFETY: geteuid has no arguments, does not write through pointers, and
    // is safe to call for querying the current process effective UID.
    unsafe { libc::geteuid() }
}

#[cfg(unix)]
fn warn_if_insecure_file_permissions(path: &Path, metadata: &fs::Metadata) {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode() & 0o777;
    if mode & !0o600 != 0 {
        eprintln!(
            "warning: auth file permissions on {} are {mode:o}; next write will repair them to 600",
            path.display()
        );
    }
}

#[cfg(not(unix))]
fn warn_if_insecure_file_permissions(_path: &Path, _metadata: &fs::Metadata) {}

#[cfg(unix)]
fn private_open_options() -> OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options.mode(0o600);
    options
}

#[cfg(not(unix))]
fn private_open_options() -> OpenOptions {
    OpenOptions::new()
}

#[cfg(unix)]
fn set_dir_mode_0700(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_dir_mode_0700(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_mode_0600(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_file_mode_0600(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[path = "tests/auth_storage.rs"]
mod tests;
