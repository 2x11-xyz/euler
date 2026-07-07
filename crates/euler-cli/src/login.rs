use anyhow::{anyhow, Result};
use euler_core::auth_storage::{
    AuthStorage, Credential as StoredCredential, SecretString as StoredSecretString,
};
use euler_provider::chatgpt::{ChatGptDeviceCode, ChatGptDeviceLogin, ChatGptLoginCredential};
use std::io::{self, Write};
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
    if provider_id != "chatgpt" {
        return Err(anyhow!("login is only supported with --provider chatgpt"));
    }
    Ok(LoginArgs { provider_id })
}

pub(crate) fn login_chatgpt(login: LoginArgs) -> Result<()> {
    let mut flow = RealChatGptLoginFlow::new();
    run_chatgpt_login(&login.provider_id, &mut flow, None, io::stdout())
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
