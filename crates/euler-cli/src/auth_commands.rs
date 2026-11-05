use anyhow::{anyhow, Result};
use euler_core::auth_storage::{AuthSource, AuthState, AuthStorage, Credential};
use euler_provider::catalog::canonical_provider_id;
use std::io::{self, Write};
use std::path::Path;

pub(crate) struct LogoutArgs {
    pub(crate) provider_id: String,
}

pub(crate) fn logout_args_for_provider(
    provider_id: String,
    auth_file_from_cli: bool,
) -> Result<LogoutArgs> {
    if auth_file_from_cli {
        return Err(anyhow!("--auth-file is not supported with logout"));
    }
    // As with login, any id that is not built-in names a custom provider.
    if canonical_provider_id(&provider_id).is_some() && provider_id != "chatgpt" {
        return Err(anyhow!("logout is only supported with --provider chatgpt"));
    }
    Ok(LogoutArgs { provider_id })
}

pub(crate) fn run_logout(logout: LogoutArgs) -> Result<()> {
    if logout.provider_id == "chatgpt" {
        return logout_chatgpt_with_path(&logout.provider_id, None, io::stdout());
    }
    logout_custom_provider_with_path(&logout.provider_id, None, io::stdout())
}

/// Removes a custom provider's stored key; its `providers.json` entry stays.
fn logout_custom_provider_with_path(
    provider_id: &str,
    storage_path: Option<&Path>,
    mut output: impl Write,
) -> Result<()> {
    let mut storage = match storage_path {
        Some(path) => AuthStorage::new(path)?,
        None => AuthStorage::new_default()?,
    };
    if !storage.contains_provider_entry(provider_id) {
        writeln!(output, "{provider_id} has no stored key.")?;
        return Ok(());
    }
    storage.remove(provider_id)?;
    writeln!(
        output,
        "Removed the stored key for {provider_id}. Its providers.json entry is unchanged."
    )?;
    Ok(())
}

fn logout_chatgpt_with_path(
    provider_id: &str,
    storage_path: Option<&Path>,
    mut output: impl Write,
) -> Result<()> {
    if provider_id != "chatgpt" {
        return Err(anyhow!("logout is only supported with --provider chatgpt"));
    }
    let mut storage = match storage_path {
        Some(path) => AuthStorage::new(path)?,
        None => AuthStorage::new_default()?,
    };
    let had_credential = storage.has("chatgpt");
    storage.remove("chatgpt")?;
    if had_credential {
        writeln!(output, "ChatGPT logout complete.")?;
    } else {
        writeln!(output, "ChatGPT was not logged in.")?;
    }
    Ok(())
}

pub(crate) fn print_auth_status() -> Result<()> {
    print_auth_status_with_path(None, io::stdout())
}

fn print_auth_status_with_path(storage_path: Option<&Path>, mut output: impl Write) -> Result<()> {
    let storage = match storage_path {
        Some(path) => AuthStorage::new(path)?,
        None => AuthStorage::new_default()?,
    };
    write_auth_status(&storage, &mut output)
}

fn write_auth_status(storage: &AuthStorage, mut output: impl Write) -> Result<()> {
    write_auth_status_line(storage, "chatgpt", None, &mut output)?;
    write_auth_status_line(storage, "anthropic", Some("ANTHROPIC_API_KEY"), &mut output)?;
    write_auth_status_line(storage, "openai", Some("OPENAI_API_KEY"), &mut output)?;
    write_auth_status_line(
        storage,
        "openrouter",
        Some("OPENROUTER_API_KEY"),
        &mut output,
    )?;
    write_auth_status_line(storage, "xai", Some("XAI_API_KEY"), &mut output)?;
    // Custom providers appear once `euler login --provider <id>` stored a key.
    for provider in storage.list() {
        if canonical_provider_id(&provider).is_none() {
            write_auth_status_line(storage, &provider, None, &mut output)?;
        }
    }
    Ok(())
}

fn write_auth_status_line(
    storage: &AuthStorage,
    provider: &str,
    env_key_name: Option<&str>,
    mut output: impl Write,
) -> Result<()> {
    let status = storage.auth_status(provider, env_key_name);
    let account_id = if provider == "chatgpt" {
        match storage.get(provider) {
            Some(Credential::OAuth { account_id, .. }) => account_id,
            _ => None,
        }
    } else {
        None
    };

    write!(
        output,
        "{provider}: configured={} source={} status={}",
        status.configured,
        source_label(status.source),
        state_label(status.state)
    )?;
    if let Some(account_id) = account_id {
        write!(output, " account_id={account_id}")?;
    }
    writeln!(output)?;
    Ok(())
}

fn source_label(source: AuthSource) -> &'static str {
    match source {
        AuthSource::Runtime => "runtime",
        AuthSource::AuthFile => "auth-file",
        AuthSource::Stored => "stored",
        AuthSource::Env => "env",
        AuthSource::Missing => "missing",
    }
}

fn state_label(state: AuthState) -> &'static str {
    match state {
        AuthState::Valid => "valid",
        AuthState::ExpiredRefreshable => "expired-refreshable",
        AuthState::ExpiredUnrefreshable => "expired-unrefreshable",
        AuthState::Missing => "missing",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use euler_core::auth_storage::SecretString;
    use std::env;
    use std::ffi::OsString;
    use tempfile::TempDir;

    struct EnvGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
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

    fn auth_path(temp: &TempDir) -> std::path::PathBuf {
        temp.path().join(".euler").join("auth.json")
    }

    fn oauth(access: &str, refresh: &str, expires: u64) -> Credential {
        Credential::OAuth {
            access: SecretString::new(access),
            refresh: SecretString::new(refresh),
            expires,
            account_id: Some("acct-1".to_owned()),
        }
    }

    fn api_key(key: &str) -> Credential {
        Credential::ApiKey {
            key: SecretString::new(key),
        }
    }

    #[test]
    fn status_output_reports_chatgpt_without_secrets() {
        let temp = TempDir::new().expect("temp dir");
        let path = auth_path(&temp);
        let mut storage = AuthStorage::new(&path).expect("storage");
        storage
            .set(
                "chatgpt",
                oauth("status-access-secret", "status-refresh-secret", u64::MAX),
            )
            .expect("set");
        storage
            .set("anthropic", api_key("status-anthropic-secret"))
            .expect("set");
        storage
            .set("openai", api_key("status-openai-secret"))
            .expect("set");
        storage
            .set("openrouter", api_key("$STATUS_OPENROUTER_SECRET_REF"))
            .expect("set");
        storage
            .set("xai", api_key("status-xai-secret"))
            .expect("set");
        let mut output = Vec::new();

        print_auth_status_with_path(Some(&path), &mut output).expect("status");
        let output = String::from_utf8(output).expect("utf8");

        assert_eq!(
            output,
            concat!(
                "chatgpt: configured=true source=stored status=valid account_id=acct-1\n",
                "anthropic: configured=true source=stored status=valid\n",
                "openai: configured=true source=stored status=valid\n",
                "openrouter: configured=true source=stored status=valid\n",
                "xai: configured=true source=stored status=valid\n",
            )
        );
        assert!(!output.contains("status-access-secret"));
        assert!(!output.contains("status-refresh-secret"));
        assert!(!output.contains("status-anthropic-secret"));
        assert!(!output.contains("status-openai-secret"));
        assert!(!output.contains("STATUS_OPENROUTER_SECRET_REF"));
        assert!(!output.contains("status-xai-secret"));
    }

    #[test]
    fn status_output_reports_missing_chatgpt() {
        let temp = TempDir::new().expect("temp dir");
        let path = auth_path(&temp);
        let mut output = Vec::new();

        print_auth_status_with_path(Some(&path), &mut output).expect("status");
        let output = String::from_utf8(output).expect("utf8");

        assert!(output.contains("chatgpt: configured=false source=missing status=missing"));
        assert!(output.contains("anthropic:"));
        assert!(output.contains("openai:"));
        assert!(output.contains("openrouter:"));
        assert!(output.contains("xai:"));
    }

    #[test]
    fn status_output_reports_openai_env_without_chatgpt_cross_auth() {
        let _lock = crate::TEST_ENV_LOCK.lock().expect("env lock");
        let _openai_env = EnvGuard::set("OPENAI_API_KEY", "status-openai-env-secret");
        let temp = TempDir::new().expect("temp dir");
        let path = auth_path(&temp);
        let mut storage = AuthStorage::new(&path).expect("storage");
        storage
            .set(
                "chatgpt",
                oauth("status-access-secret", "status-refresh-secret", u64::MAX),
            )
            .expect("set chatgpt");
        let mut output = Vec::new();

        write_auth_status(&storage, &mut output).expect("status");
        let output = String::from_utf8(output).expect("utf8");

        assert!(output.contains("chatgpt: configured=true source=stored status=valid"));
        assert!(output.contains("openai: configured=true source=env status=valid"));
        assert!(!output.contains("status-openai-env-secret"));
        assert!(!output.contains("status-access-secret"));
        assert!(!output.contains("status-refresh-secret"));
    }

    #[test]
    fn status_output_reports_openai_missing_when_only_chatgpt_is_stored() {
        let _lock = crate::TEST_ENV_LOCK.lock().expect("env lock");
        let _openai_env = EnvGuard::set("OPENAI_API_KEY", "");
        let temp = TempDir::new().expect("temp dir");
        let path = auth_path(&temp);
        let mut storage = AuthStorage::new(&path).expect("storage");
        storage
            .set(
                "chatgpt",
                oauth("status-access-secret", "status-refresh-secret", u64::MAX),
            )
            .expect("set chatgpt");
        let mut output = Vec::new();

        write_auth_status(&storage, &mut output).expect("status");
        let output = String::from_utf8(output).expect("utf8");

        assert!(output.contains("chatgpt: configured=true source=stored status=valid"));
        assert!(output.contains("openai: configured=false source=missing status=missing"));
        assert!(!output.contains("status-access-secret"));
        assert!(!output.contains("status-refresh-secret"));
    }

    #[test]
    fn logout_removes_chatgpt_without_printing_secrets() {
        let temp = TempDir::new().expect("temp dir");
        let path = auth_path(&temp);
        let mut storage = AuthStorage::new(&path).expect("storage");
        storage
            .set(
                "chatgpt",
                oauth("logout-access-secret", "logout-refresh-secret", u64::MAX),
            )
            .expect("set");
        let mut output = Vec::new();

        logout_chatgpt_with_path("chatgpt", Some(&path), &mut output).expect("logout");
        let output = String::from_utf8(output).expect("utf8");

        assert_eq!(output, "ChatGPT logout complete.\n");
        assert!(!output.contains("logout-access-secret"));
        assert!(!output.contains("logout-refresh-secret"));
        assert!(!AuthStorage::new(&path).expect("storage").has("chatgpt"));
    }

    #[test]
    fn status_output_lists_stored_custom_provider_keys_without_secrets() {
        let temp = TempDir::new().expect("temp dir");
        let path = auth_path(&temp);
        let mut storage = AuthStorage::new(&path).expect("storage");
        storage
            .set("gw", api_key("status-gateway-secret"))
            .expect("set");
        let mut output = Vec::new();

        print_auth_status_with_path(Some(&path), &mut output).expect("status");
        let output = String::from_utf8(output).expect("utf8");

        assert!(
            output.ends_with("gw: configured=true source=stored status=valid\n"),
            "{output}"
        );
        assert!(!output.contains("status-gateway-secret"));
    }

    #[test]
    fn custom_provider_logout_removes_only_that_key() {
        let temp = TempDir::new().expect("temp dir");
        let path = auth_path(&temp);
        let mut storage = AuthStorage::new(&path).expect("storage");
        storage
            .set("gw", api_key("logout-gateway-secret"))
            .expect("set gw");
        storage
            .set("chatgpt", oauth("access", "refresh", u64::MAX))
            .expect("set chatgpt");
        let mut output = Vec::new();

        logout_custom_provider_with_path("gw", Some(&path), &mut output).expect("logout");
        logout_custom_provider_with_path("gw", Some(&path), &mut output).expect("logout again");

        assert_eq!(
            String::from_utf8(output).expect("utf8"),
            "Removed the stored key for gw. Its providers.json entry is unchanged.\ngw has no stored key.\n"
        );
        let storage = AuthStorage::new(&path).expect("storage");
        assert!(!storage.has("gw"));
        assert!(storage.has("chatgpt"));
    }

    #[test]
    fn logout_missing_chatgpt_is_idempotent() {
        let temp = TempDir::new().expect("temp dir");
        let path = auth_path(&temp);
        let mut output = Vec::new();

        logout_chatgpt_with_path("chatgpt", Some(&path), &mut output).expect("logout");

        assert_eq!(
            String::from_utf8(output).expect("utf8"),
            "ChatGPT was not logged in.\n"
        );
    }
}
