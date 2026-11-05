use anyhow::{anyhow, Result};
use euler_core::auth_storage::{
    AuthStorage, Credential as StoredCredential, SecretString as StoredSecretString,
};
use euler_provider::catalog::canonical_provider_id;
use euler_provider::chatgpt::{ChatGptDeviceCode, ChatGptDeviceLogin, ChatGptLoginCredential};
use euler_provider::provider_config::ProviderConfigRegistry;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

pub(crate) struct LoginArgs {
    pub(crate) provider_id: String,
}

pub(crate) fn login_args_for_provider(
    provider_id: String,
    auth_file_from_cli: bool,
) -> Result<LoginArgs> {
    if auth_file_from_cli {
        return Err(anyhow!("--auth-file is not supported with login"));
    }
    // Built-in ids other than chatgpt keep their environment-key setup; any
    // other id names a custom provider from providers.json.
    if canonical_provider_id(&provider_id).is_some() && provider_id != "chatgpt" {
        return Err(anyhow!(
            "login supports chatgpt and custom providers from providers.json, not built-in provider {provider_id}"
        ));
    }
    Ok(LoginArgs { provider_id })
}

pub(crate) fn run_login(login: LoginArgs) -> Result<()> {
    if login.provider_id == "chatgpt" {
        let mut flow = RealChatGptLoginFlow::new();
        return run_chatgpt_login(&login.provider_id, &mut flow, None, io::stdout());
    }
    let provider_config = crate::provider_config_runtime::resolve_provider_config_path()?;
    let custom_providers =
        crate::cli::providers::load_custom_provider_config(Some(&provider_config));
    run_custom_provider_login(
        &login.provider_id,
        &custom_providers,
        &provider_config,
        None,
        read_hidden_key,
        io::stdout(),
    )
}

/// Stores an API key for an existing `providers.json` entry that sends
/// `Authorization: Bearer` (`auth_header: true`) and sets no `api_key` spec.
fn run_custom_provider_login(
    provider_id: &str,
    custom_providers: &ProviderConfigRegistry,
    provider_config: &Path,
    storage_path: Option<&Path>,
    read_key: impl FnOnce() -> Result<String>,
    mut output: impl Write,
) -> Result<()> {
    let config = provider_config.display();
    let Some(provider) = custom_providers.provider(provider_id) else {
        return Err(anyhow!(
            "no provider {provider_id} in {config}; add an entry with auth_header: true, then run euler login --provider {provider_id}"
        ));
    };
    if provider.api_key.is_some() {
        return Err(anyhow!(
            "provider {provider_id} sets api_key in {config}; remove api_key to use a stored key"
        ));
    }
    if !provider.auth_header {
        return Err(anyhow!(
            "provider {provider_id} does not set auth_header: true in {config}, so a stored key would not be sent; set auth_header: true"
        ));
    }
    write!(output, "API key: ")?;
    output.flush()?;
    let key = read_key()?;
    let key = key.trim();
    if key.is_empty() {
        return Err(anyhow!("no API key entered; nothing was stored"));
    }
    let auth_path = match storage_path {
        Some(path) => path.to_path_buf(),
        None => AuthStorage::default_path()?,
    };
    AuthStorage::new(&auth_path)?.set(
        provider_id,
        StoredCredential::ApiKey {
            key: StoredSecretString::new(escape_stored_api_key(key)),
        },
    )?;
    writeln!(
        output,
        "Stored the key for {provider_id} in {}.",
        auth_path.display()
    )?;
    Ok(())
}

/// `AuthStorage::resolve_api_key` reads stored keys as config values, where a
/// leading `$` names an environment variable and a leading `!` a command. A
/// typed key is a literal, so one starting with either character gets a `$`
/// escape (`$$...` reads back as `$...`, `$!...` as `!...`).
fn escape_stored_api_key(key: &str) -> String {
    if key.starts_with('$') || key.starts_with('!') {
        format!("${key}")
    } else {
        key.to_owned()
    }
}

/// Reads the key without echo when stdin is a terminal, and as one line
/// otherwise, so a key can also be piped in.
fn read_hidden_key() -> Result<String> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        let mut line = String::new();
        stdin.read_line(&mut line)?;
        return Ok(line);
    }
    crossterm::terminal::enable_raw_mode()?;
    let key = read_key_until_enter(stdin.lock());
    crossterm::terminal::disable_raw_mode()?;
    println!();
    key
}

/// Raw-mode line reading: Enter ends the key, Backspace deletes, and Ctrl-C
/// or Ctrl-D cancels.
fn read_key_until_enter(input: impl BufRead) -> Result<String> {
    let mut key = Vec::new();
    for byte in input.bytes() {
        match byte? {
            b'\r' | b'\n' => break,
            0x03 | 0x04 => return Err(anyhow!("login cancelled; nothing was stored")),
            0x08 | 0x7f => {
                key.pop();
            }
            byte => key.push(byte),
        }
    }
    String::from_utf8(key).map_err(|_| anyhow!("the API key is not valid UTF-8"))
}

trait ChatGptLoginFlow {
    fn start(&mut self) -> Result<DisplayedDeviceCode>;
    fn finish(&mut self) -> Result<ChatGptLoginCredential>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DisplayedDeviceCode {
    verification_url: String,
    user_code: String,
    expires_in_seconds: u64,
}

struct RealChatGptLoginFlow {
    inner: ChatGptDeviceLogin,
    device: Option<ChatGptDeviceCode>,
}

impl RealChatGptLoginFlow {
    fn new() -> Self {
        Self {
            inner: ChatGptDeviceLogin::new(),
            device: None,
        }
    }
}

impl ChatGptLoginFlow for RealChatGptLoginFlow {
    fn start(&mut self) -> Result<DisplayedDeviceCode> {
        let device = self.inner.start()?;
        let displayed = DisplayedDeviceCode {
            verification_url: device.verification_url().to_owned(),
            user_code: device.user_code().to_owned(),
            expires_in_seconds: device.expires_in_seconds(),
        };
        self.device = Some(device);
        Ok(displayed)
    }

    fn finish(&mut self) -> Result<ChatGptLoginCredential> {
        let device = self
            .device
            .as_ref()
            .ok_or_else(|| anyhow!("ChatGPT login was not started"))?;
        self.inner.finish(device).map_err(Into::into)
    }
}

fn run_chatgpt_login(
    provider_id: &str,
    flow: &mut impl ChatGptLoginFlow,
    storage_path: Option<&Path>,
    mut output: impl Write,
) -> Result<()> {
    if provider_id != "chatgpt" {
        return Err(anyhow!("login is only supported with --provider chatgpt"));
    }

    let device = flow.start()?;
    writeln!(
        output,
        "Open this URL to authenticate: {}",
        device.verification_url
    )?;
    writeln!(output, "Enter this code: {}", device.user_code)?;
    writeln!(
        output,
        "Code expires in {}.",
        format_expiry(device.expires_in_seconds)
    )?;
    writeln!(output, "Waiting for authorization...")?;

    let credential = flow.finish()?;
    save_chatgpt_credential(storage_path, credential)?;
    writeln!(output, "ChatGPT login saved.")?;
    Ok(())
}

fn save_chatgpt_credential(
    storage_path: Option<&Path>,
    credential: ChatGptLoginCredential,
) -> Result<()> {
    let mut storage = match storage_path {
        Some(path) => AuthStorage::new(path)?,
        None => AuthStorage::new_default()?,
    };
    let parts = credential.into_storage_parts();
    storage.set(
        "chatgpt",
        StoredCredential::OAuth {
            access: StoredSecretString::new(parts.access),
            refresh: StoredSecretString::new(parts.refresh),
            expires: parts.expires,
            account_id: parts.account_id,
        },
    )?;
    Ok(())
}

fn format_expiry(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{} second{}", seconds, if seconds == 1 { "" } else { "s" });
    }

    let minutes = seconds / 60;
    let remainder = seconds % 60;
    if remainder == 0 {
        return format!("{} minute{}", minutes, if minutes == 1 { "" } else { "s" });
    }

    format!(
        "{} minute{} {} second{}",
        minutes,
        if minutes == 1 { "" } else { "s" },
        remainder,
        if remainder == 1 { "" } else { "s" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_writes_chatgpt_oauth_credential_to_injected_storage_path() {
        let temp = tempfile::tempdir().expect("temp dir");
        let auth_path = temp.path().join("auth.json");
        let mut flow = FakeLoginFlow::success();
        let mut output = Vec::new();

        run_chatgpt_login("chatgpt", &mut flow, Some(&auth_path), &mut output).expect("login");

        let storage = AuthStorage::new(&auth_path).expect("storage");
        let credential = storage.get("chatgpt").expect("chatgpt credential");
        match credential {
            StoredCredential::OAuth {
                access,
                refresh,
                expires,
                account_id,
            } => {
                assert_eq!(access.expose_secret(), "access-secret");
                assert_eq!(refresh.expose_secret(), "refresh-secret");
                assert_eq!(expires, 123_456);
                assert_eq!(account_id.as_deref(), Some("account-123"));
            }
            StoredCredential::ApiKey { .. } => panic!("expected OAuth credential"),
        }

        let output = String::from_utf8(output).expect("utf8");
        assert!(output.contains("https://auth.openai.com/codex/device"));
        assert!(output.contains("USER-CODE"));
        assert!(output.contains("Code expires in 15 minutes."));
        assert!(output.contains("ChatGPT login saved."));
    }

    #[test]
    fn expiry_display_uses_seconds_for_short_durations() {
        assert_eq!(format_expiry(1), "1 second");
        assert_eq!(format_expiry(30), "30 seconds");
        assert_eq!(format_expiry(90), "1 minute 30 seconds");
    }

    #[test]
    fn login_failure_does_not_write_partial_credential() {
        let temp = tempfile::tempdir().expect("temp dir");
        let auth_path = temp.path().join("auth.json");
        let mut flow = FakeLoginFlow::failure();
        let mut output = Vec::new();

        let error = run_chatgpt_login("chatgpt", &mut flow, Some(&auth_path), &mut output)
            .expect_err("login failure");

        assert_eq!(error.to_string(), "authorization denied");
        assert!(!auth_path.exists());
    }

    fn registry(entry: &str) -> ProviderConfigRegistry {
        let (registry, warnings) =
            ProviderConfigRegistry::with_json(&format!(r#"{{"providers": {{"gw": {entry}}}}}"#));
        assert!(warnings.is_empty(), "{warnings:?}");
        registry
    }

    const BEARER_ENTRY: &str = r#"{"api_family": "openai_chat_completions", "base_url": "https://gateway.example.com/v1", "auth_header": true, "default_model": "m"}"#;

    fn custom_login(
        entry: &str,
        provider_id: &str,
        key: &str,
        auth_path: &Path,
    ) -> (Result<()>, String) {
        let mut output = Vec::new();
        let key = key.to_owned();
        let result = run_custom_provider_login(
            provider_id,
            &registry(entry),
            Path::new("/home/user/.euler/providers.json"),
            Some(auth_path),
            move || Ok(key),
            &mut output,
        );
        (result, String::from_utf8(output).expect("utf8"))
    }

    #[test]
    fn custom_provider_login_stores_trimmed_key_as_api_key_credential() {
        let temp = tempfile::tempdir().expect("temp dir");
        let auth_path = temp.path().join("auth.json");

        let (result, output) = custom_login(BEARER_ENTRY, "gw", " gateway-secret\n", &auth_path);

        result.expect("login");
        assert_eq!(
            output,
            format!(
                "API key: Stored the key for gw in {}.\n",
                auth_path.display()
            )
        );
        match AuthStorage::new(&auth_path).expect("storage").get("gw") {
            Some(StoredCredential::ApiKey { key }) => {
                assert_eq!(key.expose_secret(), "gateway-secret")
            }
            other => panic!("expected api key credential, got {other:?}"),
        }
    }

    #[test]
    fn custom_provider_login_stores_keys_starting_with_dollar_or_bang_as_literals() {
        for key in [
            "$gateway-secret",
            "!gateway-secret",
            "${GATEWAY}",
            "$$x",
            "$!x",
            "$",
            "!",
        ] {
            let temp = tempfile::tempdir().expect("temp dir");
            let auth_path = temp.path().join("auth.json");

            let (result, _) = custom_login(BEARER_ENTRY, "gw", key, &auth_path);

            result.expect("login");
            let resolved = AuthStorage::new(&auth_path)
                .expect("storage")
                .resolve_api_key("gw", None);
            assert_eq!(
                resolved.as_ref().map(|key| key.expose_secret()),
                Some(key),
                "{key}"
            );
        }
    }

    #[test]
    fn custom_provider_login_refusals_store_nothing() {
        let without_auth_header = r#"{"api_family": "openai_chat_completions", "base_url": "https://gateway.example.com/v1", "default_model": "m"}"#;
        let cases = [
            (BEARER_ENTRY, "other", "key", "no provider other in /home/user/.euler/providers.json; add an entry with auth_header: true, then run euler login --provider other"),
            (without_auth_header, "gw", "key", "provider gw does not set auth_header: true in /home/user/.euler/providers.json, so a stored key would not be sent; set auth_header: true"),
            (BEARER_ENTRY, "gw", "  \n", "no API key entered; nothing was stored"),
        ];
        for (entry, provider_id, key, expected) in cases {
            let temp = tempfile::tempdir().expect("temp dir");
            let auth_path = temp.path().join("auth.json");

            let (result, _) = custom_login(entry, provider_id, key, &auth_path);

            assert_eq!(result.expect_err("refused").to_string(), expected);
            assert!(!auth_path.exists(), "{expected}");
        }
    }

    #[test]
    fn hidden_key_reader_handles_backspace_and_cancel() {
        assert_eq!(
            read_key_until_enter(&b"sk-ab\x7fc\rignored"[..]).expect("key"),
            "sk-ac"
        );
        assert_eq!(
            read_key_until_enter(&b"sk-abc"[..]).expect("key at EOF"),
            "sk-abc"
        );
        assert_eq!(
            read_key_until_enter(&b"sk-\x03"[..])
                .expect_err("ctrl-c")
                .to_string(),
            "login cancelled; nothing was stored"
        );
    }

    struct FakeLoginFlow {
        fail: bool,
    }

    impl FakeLoginFlow {
        fn success() -> Self {
            Self { fail: false }
        }

        fn failure() -> Self {
            Self { fail: true }
        }
    }

    impl ChatGptLoginFlow for FakeLoginFlow {
        fn start(&mut self) -> Result<DisplayedDeviceCode> {
            Ok(DisplayedDeviceCode {
                verification_url: "https://auth.openai.com/codex/device".to_owned(),
                user_code: "USER-CODE".to_owned(),
                expires_in_seconds: 900,
            })
        }

        fn finish(&mut self) -> Result<ChatGptLoginCredential> {
            if self.fail {
                return Err(anyhow!("authorization denied"));
            }
            Ok(ChatGptLoginCredential {
                access: euler_provider::auth::SecretString::new("access-secret"),
                refresh: euler_provider::auth::SecretString::new("refresh-secret"),
                expires: 123_456,
                account_id: Some("account-123".to_owned()),
            })
        }
    }
}
