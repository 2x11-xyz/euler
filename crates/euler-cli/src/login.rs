use anyhow::{anyhow, Result};
use euler_core::auth_storage::{
    AuthStorage, Credential as StoredCredential, SecretString as StoredSecretString,
};
use euler_provider::auth::SecretString as ProviderSecretString;
use euler_provider::catalog::canonical_provider_id;
use euler_provider::chatgpt::{ChatGptDeviceCode, ChatGptDeviceLogin, ChatGptLoginCredential};
use euler_provider::custom_provider::CustomOpenAiProvider;
use euler_provider::gateway_discovery::GatewayDiscovery;
use euler_provider::provider_config::{
    custom_provider_id_is_valid, login_base_url, ApiFamily, CustomProviderConfig,
    ProviderConfigRegistry, CUSTOM_PROVIDER_ID_RULE,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;

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
    let providers = crate::cli::providers::load_custom_provider_config(Some(&provider_config));
    let auth_path = AuthStorage::default_path()?;
    run_custom_provider_login(
        &CustomProviderLogin {
            provider_id: &login.provider_id,
            providers: &providers,
            provider_config: &provider_config,
            auth_path: &auth_path,
        },
        &mut TerminalInput,
        io::stdout(),
        io::stderr(),
    )
}

/// What `euler login --provider <custom id>` reads: visible lines (the base
/// URL and the save-anyway answer) and the hidden key. The flow writes each
/// prompt to its output first.
trait LoginInput {
    fn line(&mut self) -> Result<String>;
    fn hidden_line(&mut self) -> Result<String>;
}

struct TerminalInput;

impl LoginInput for TerminalInput {
    fn line(&mut self) -> Result<String> {
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        Ok(line)
    }

    fn hidden_line(&mut self) -> Result<String> {
        read_hidden_key()
    }
}

struct CustomProviderLogin<'a> {
    provider_id: &'a str,
    providers: &'a ProviderConfigRegistry,
    provider_config: &'a Path,
    auth_path: &'a Path,
}

/// Login for a custom provider (ADR 0025 section 1). It prompts for the base
/// URL and the key, verifies them with gateway discovery, then stores the key,
/// writes the `providers.json` entry, and writes the discovery cache. A new id
/// gets a new entry; an existing entry gains the base URL and
/// `discovery: true`. When an existing entry's gateway lists no models and
/// the base URL was kept, the key can still be stored with the entry left as
/// written.
fn run_custom_provider_login(
    login: &CustomProviderLogin<'_>,
    input: &mut impl LoginInput,
    mut output: impl Write,
    warnings: impl Write,
) -> Result<()> {
    let provider_id = login.provider_id;
    if !custom_provider_id_is_valid(provider_id) {
        return Err(anyhow!(
            "provider id `{provider_id}` is not valid: {CUSTOM_PROVIDER_ID_RULE}; nothing was stored"
        ));
    }
    let mut document = read_provider_document(login.provider_config)?;
    let existing = existing_login_entry(login, &document)?;
    let base_url = prompt_base_url(existing, input, &mut output)?;
    write!(output, "API key: ")?;
    output.flush()?;
    let key = input.hidden_line()?;
    let key = key.trim().to_owned();
    if key.is_empty() {
        return Err(anyhow!("no API key entered; nothing was stored"));
    }

    let provider = match existing {
        Some(provider) => CustomProviderConfig {
            base_url: base_url.clone(),
            discovery: true,
            ..provider.clone()
        },
        None => CustomProviderConfig {
            id: provider_id.to_owned(),
            api_family: ApiFamily::OpenAiChatCompletions,
            base_url: base_url.clone(),
            api_key: None,
            auth_header: true,
            headers: BTreeMap::new(),
            default_model: None,
            default_model_error: None,
            models: BTreeMap::new(),
            discovery: true,
            discovery_source: None,
        },
    };
    let typed_key = ProviderSecretString::new(key.clone());
    let discovery = CustomOpenAiProvider::from_config(provider)
        .map_err(|error| anyhow!("{error}"))?
        .with_stored_api_key(Arc::new(move |_| Ok(Some(typed_key.clone()))))
        .discover_models();
    match discovery {
        Ok(discovery) => {
            let picked = login_default_model(existing, &discovery);
            let default_model = set_provider_entry(&mut document, provider_id, &base_url, picked);
            save_verified_login(login, &key, &document, &discovery, &mut output, warnings)?;
            if let Some(model) = default_model {
                writeln!(
                    output,
                    "Default model: {model} (change with --model or /model)"
                )?;
            }
            Ok(())
        }
        Err(error)
            if !error.key_rejected
                && existing.is_some_and(|provider| {
                    login_base_url(&provider.base_url).as_deref() == Ok(base_url.as_str())
                }) =>
        {
            writeln!(output, "Model discovery failed: {error}.")?;
            write!(output, "Save the key without model discovery? [Y/n] ")?;
            output.flush()?;
            let answer = input.line()?;
            if !matches!(answer.trim(), "" | "y" | "Y" | "yes" | "Yes") {
                return Err(anyhow!("login cancelled; nothing was stored"));
            }
            store_api_key(login.auth_path, provider_id, &key)?;
            writeln!(
                output,
                "Stored the key for {provider_id} in {}. Its providers.json entry is unchanged, so the key is sent to {}.",
                login.auth_path.display(),
                existing.map_or("", |provider| provider.base_url.as_str())
            )?;
            Ok(())
        }
        Err(error) => Err(anyhow!(
            "could not verify provider {provider_id}: {error}; nothing was stored"
        )),
    }
}

/// The model login sets as `default_model`, picked from the discovered
/// models with the entry's configured `models` replacing a discovered one
/// whole, as a launch lists them.
fn login_default_model<'a>(
    existing: Option<&'a CustomProviderConfig>,
    discovery: &'a GatewayDiscovery,
) -> Option<&'a str> {
    let mut models = discovery
        .models
        .iter()
        .map(|model| (model.id.as_str(), model))
        .collect::<BTreeMap<_, _>>();
    let configured = existing
        .into_iter()
        .flat_map(|provider| provider.models.values())
        .filter(|model| !model.discovered);
    models.extend(configured.map(|model| (model.id.as_str(), model)));
    crate::provider_discovery::pick_default_model(models.values().copied())
}

/// The loaded entry login updates, or `None` for a new id. Refuses an entry
/// with an `api_key` spec or without `auth_header: true`, and an entry in the
/// file that failed to load.
fn existing_login_entry<'a>(
    login: &CustomProviderLogin<'a>,
    document: &Value,
) -> Result<Option<&'a CustomProviderConfig>> {
    let provider_id = login.provider_id;
    let config = login.provider_config.display();
    match login.providers.provider(provider_id) {
        Some(provider) if provider.api_key.is_some() => Err(anyhow!(
            "provider {provider_id} sets api_key in {config}; remove api_key to use a stored key"
        )),
        Some(provider) if !provider.auth_header => Err(anyhow!(
            "provider {provider_id} does not set auth_header: true in {config}, so a stored key would not be sent; set auth_header: true"
        )),
        None if document["providers"].get(provider_id).is_some() => Err(anyhow!(
            "provider {provider_id} in {config} is not valid (see the warning above); fix or remove it, then run euler login --provider {provider_id}"
        )),
        existing => Ok(existing),
    }
}

/// Prompts for the base URL (an existing entry's is the default), then
/// normalizes and prints it.
fn prompt_base_url(
    existing: Option<&CustomProviderConfig>,
    input: &mut impl LoginInput,
    mut output: impl Write,
) -> Result<String> {
    match existing {
        Some(provider) => write!(output, "Base URL [{}]: ", provider.base_url)?,
        None => write!(output, "Base URL: ")?,
    }
    output.flush()?;
    let typed = input.line()?;
    let typed = match (typed.trim(), existing) {
        ("", Some(provider)) => provider.base_url.clone(),
        ("", None) => return Err(anyhow!("no base URL entered; nothing was stored")),
        (typed, _) => typed.to_owned(),
    };
    let base_url = login_base_url(&typed).map_err(|error| anyhow!(error))?;
    writeln!(output, "Using {base_url}")?;
    Ok(base_url)
}

/// Stores the key, then writes `document` as `providers.json`, then the
/// discovery cache. The key goes first so a provider never points at a
/// missing key; a failed entry write puts the previous key back, and a failed
/// cache write only warns.
fn save_verified_login(
    login: &CustomProviderLogin<'_>,
    key: &str,
    document: &Value,
    discovery: &GatewayDiscovery,
    mut output: impl Write,
    mut warnings: impl Write,
) -> Result<()> {
    let provider_id = login.provider_id;
    let config = login.provider_config.display();
    let previous_key = store_api_key(login.auth_path, provider_id, key)?;
    if let Err(error) = write_provider_document(login.provider_config, document) {
        let restored =
            AuthStorage::new(login.auth_path).and_then(|mut storage| match previous_key {
                Some(previous) => storage.set(provider_id, previous),
                None => storage.remove(provider_id),
            });
        return Err(match restored {
            Ok(()) => anyhow!("{error:#}; the key for {provider_id} was not stored"),
            Err(restore_error) => anyhow!(
                "{error:#}; restoring the previous key for {provider_id} in {} also failed: {restore_error}",
                login.auth_path.display()
            ),
        });
    }
    let base_url = document["providers"][provider_id]["base_url"]
        .as_str()
        .expect("set_provider_entry writes base_url");
    if let Err(error) = crate::provider_discovery::write_cache(
        &crate::provider_discovery::cache_dir(login.provider_config),
        provider_id,
        base_url,
        discovery,
        chrono::Utc::now(),
    ) {
        writeln!(
            warnings,
            "warning: {error:#}; the next launch fetches the models"
        )?;
    }
    writeln!(
        output,
        "Found {}.",
        crate::provider_discovery::family_counts(discovery)
    )?;
    for note in &discovery.notes {
        writeln!(
            warnings,
            "warning: discovery for provider `{provider_id}` {note}"
        )?;
    }
    if login.providers.provider(provider_id).is_some() {
        writeln!(output, "Updated provider {provider_id} in {config}.")?;
    } else {
        writeln!(output, "Added provider {provider_id} to {config}.")?;
    }
    writeln!(
        output,
        "Stored the key for {provider_id} in {}.",
        login.auth_path.display()
    )?;
    Ok(())
}

/// Stores `key` as the provider's API-key credential and returns the
/// credential it replaced.
fn store_api_key(
    auth_path: &Path,
    provider_id: &str,
    key: &str,
) -> Result<Option<StoredCredential>> {
    let mut storage = AuthStorage::new(auth_path)?;
    let previous = storage.get(provider_id);
    storage.set(
        provider_id,
        StoredCredential::ApiKey {
            key: StoredSecretString::new(escape_stored_api_key(key)),
        },
    )?;
    Ok(previous)
}

/// `providers.json` as a JSON document. A missing or blank file is an empty
/// document; a file that fails to parse is an error naming it.
fn read_provider_document(path: &Path) -> Result<Value> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(anyhow!("could not read {}: {error}", path.display())),
    };
    let contents = contents.trim_start_matches('\u{feff}');
    if contents.trim().is_empty() {
        return Ok(json!({}));
    }
    let document: Value = serde_json::from_str(contents).map_err(|error| {
        anyhow!(
            "{} is not valid JSON ({error}); fix it, then run euler login again",
            path.display()
        )
    })?;
    if !document.is_object() {
        return Err(anyhow!("{} is not a JSON object", path.display()));
    }
    if document
        .get("providers")
        .is_some_and(|providers| !providers.is_object())
    {
        return Err(anyhow!("providers in {} is not an object", path.display()));
    }
    Ok(document)
}

/// Adds the entry login writes, or sets `base_url` and `discovery: true` on an
/// existing entry and leaves its other fields as written. An entry without a
/// `default_model` gets `picked_model`; an existing one is never replaced.
/// Returns the `default_model` it set.
fn set_provider_entry<'a>(
    document: &mut Value,
    provider_id: &str,
    base_url: &str,
    picked_model: Option<&'a str>,
) -> Option<&'a str> {
    let providers = document
        .as_object_mut()
        .expect("read_provider_document checks the root")
        .entry("providers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("read_provider_document checks providers");
    let entry = match providers
        .get_mut(provider_id)
        .and_then(Value::as_object_mut)
    {
        Some(entry) => {
            entry.insert("base_url".to_owned(), json!(base_url));
            entry.insert("discovery".to_owned(), json!(true));
            entry
        }
        None => {
            providers.insert(
                provider_id.to_owned(),
                json!({
                    "api_family": ApiFamily::OpenAiChatCompletions.as_str(),
                    "base_url": base_url,
                    "auth_header": true,
                    "discovery": true,
                }),
            );
            providers[provider_id]
                .as_object_mut()
                .expect("the entry just inserted")
        }
    };
    if entry.contains_key("default_model") {
        return None;
    }
    let model = picked_model?;
    entry.insert("default_model".to_owned(), json!(model));
    Some(model)
}

/// Replaces `providers.json` atomically through a temp file in the same
/// directory, keeping the file's permissions (owner-only for a new file).
fn write_provider_document(path: &Path, document: &Value) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    crate::provider_discovery::create_private_dir(dir)?;
    let permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let mut bytes = serde_json::to_vec_pretty(document)?;
    bytes.push(b'\n');
    let temp = dir.join(format!(".providers.json-{}.tmp", std::process::id()));
    let written = (|| -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&temp)?;
        file.write_all(&bytes)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written.map_err(|error| anyhow!("could not write {}: {error:#}", path.display()))
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

    const MODEL_INFO: &str = r#"{"data": [
        {"model_name": "claude-sonnet", "litellm_params": {"model": "anthropic/claude-sonnet"},
         "model_info": {"litellm_provider": "anthropic", "mode": "chat"}},
        {"model_name": "gpt-4o", "litellm_params": {"model": "openai/gpt-4o"},
         "model_info": {"litellm_provider": "openai", "mode": "chat"}}
    ]}"#;

    /// Answers `GET <path>` with the matching status and body (404 for other
    /// paths) on every connection, and reports each request's head.
    fn spawn_gateway(
        routes: &'static [(&'static str, u16, &'static str)],
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("addr");
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut head = Vec::new();
                let mut byte = [0_u8; 1];
                while !head.ends_with(b"\r\n\r\n")
                    && io::Read::read(&mut stream, &mut byte).unwrap_or(0) == 1
                {
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let (status, body) = routes
                    .iter()
                    .find(|(path, _, _)| head.starts_with(&format!("GET {path} ")))
                    .map_or((404, "{}"), |(_, status, body)| (*status, *body));
                let _ = sender.send(head);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (format!("http://{address}"), receiver)
    }

    struct ScriptedInput(Vec<&'static str>);

    impl LoginInput for ScriptedInput {
        fn line(&mut self) -> Result<String> {
            Ok(if self.0.is_empty() {
                String::new()
            } else {
                self.0.remove(0).to_owned()
            })
        }

        fn hidden_line(&mut self) -> Result<String> {
            self.line()
        }
    }

    struct LoginHome {
        _temp: tempfile::TempDir,
        providers_path: std::path::PathBuf,
        auth_path: std::path::PathBuf,
    }

    impl LoginHome {
        fn new(providers_json: Option<&str>) -> Self {
            let temp = tempfile::tempdir().expect("temp dir");
            let providers_path = temp.path().join("providers.json");
            if let Some(contents) = providers_json {
                fs::write(&providers_path, contents).expect("providers.json");
            }
            let auth_path = temp.path().join("auth.json");
            Self {
                _temp: temp,
                providers_path,
                auth_path,
            }
        }

        fn login(
            &self,
            provider_id: &str,
            input: Vec<&'static str>,
        ) -> (Result<()>, String, String) {
            let contents = fs::read_to_string(&self.providers_path).unwrap_or_default();
            let (providers, _) = ProviderConfigRegistry::with_json(&contents);
            let (mut output, mut warnings) = (Vec::new(), Vec::new());
            let result = run_custom_provider_login(
                &CustomProviderLogin {
                    provider_id,
                    providers: &providers,
                    provider_config: &self.providers_path,
                    auth_path: &self.auth_path,
                },
                &mut ScriptedInput(input),
                &mut output,
                &mut warnings,
            );
            (
                result,
                String::from_utf8(output).expect("utf8"),
                String::from_utf8(warnings).expect("utf8"),
            )
        }

        fn providers(&self) -> Value {
            serde_json::from_str(&fs::read_to_string(&self.providers_path).expect("providers.json"))
                .expect("json")
        }

        fn stored_key(&self, provider_id: &str) -> Option<String> {
            AuthStorage::new(&self.auth_path)
                .expect("storage")
                .resolve_api_key(provider_id, None)
                .map(|key| key.expose_secret().to_owned())
        }

        fn cache_path(&self, provider_id: &str) -> std::path::PathBuf {
            crate::provider_discovery::cache_dir(&self.providers_path)
                .join(format!("{provider_id}.json"))
        }
    }

    fn leak(text: String) -> &'static str {
        Box::leak(text.into_boxed_str())
    }

    #[test]
    fn custom_provider_login_for_a_new_id_verifies_then_writes_key_entry_and_cache() {
        static ROUTES: &[(&str, u16, &str)] = &[("/model/info", 200, MODEL_INFO)];
        let (root, requests) = spawn_gateway(ROUTES);
        let home = LoginHome::new(Some(
            r#"{"version": 1, "providers": {"other": {"api_family": "openai_chat_completions", "base_url": "https://other.example.com/v1", "api_key": "$OTHER_KEY", "default_model": "m"}}}"#,
        ));

        let (result, output, warnings) = home.login(
            "gw",
            vec![leak(format!("{root}/v1/\n")), " gateway-secret\n"],
        );

        result.expect("login");
        assert_eq!(warnings, "");
        assert_eq!(
            output,
            format!(
                "Base URL: Using {root}/v1\nAPI key: Found 2 models from /model/info: 1 anthropic_messages, 1 openai_chat_completions.\nAdded provider gw to {}.\nStored the key for gw in {}.\nDefault model: claude-sonnet (change with --model or /model)\n",
                home.providers_path.display(),
                home.auth_path.display()
            )
        );
        assert_eq!(
            home.providers(),
            json!({"version": 1, "providers": {
                "other": {"api_family": "openai_chat_completions", "base_url": "https://other.example.com/v1", "api_key": "$OTHER_KEY", "default_model": "m"},
                "gw": {"api_family": "openai_chat_completions", "base_url": format!("{root}/v1"), "auth_header": true, "discovery": true, "default_model": "claude-sonnet"}
            }})
        );
        assert_eq!(home.stored_key("gw").as_deref(), Some("gateway-secret"));
        let cache = fs::read_to_string(home.cache_path("gw")).expect("cache");
        assert!(cache.contains("claude-sonnet"), "{cache}");
        let head = requests.recv().expect("request").to_ascii_lowercase();
        assert!(
            head.contains("authorization: bearer gateway-secret"),
            "{head}"
        );
    }

    #[test]
    fn custom_provider_login_updates_an_existing_entry_and_keeps_its_other_fields() {
        static ROUTES: &[(&str, u16, &str)] = &[("/model/info", 200, MODEL_INFO)];
        let (root, _requests) = spawn_gateway(ROUTES);
        let home = LoginHome::new(Some(leak(format!(
            r#"{{"providers": {{"gw": {{"api_family": "openai_chat_completions", "base_url": "{root}", "auth_header": true, "headers": {{"x-team": "research"}}, "default_model": "gpt-4o"}}}}}}"#
        ))));

        let (result, output, _) = home.login("gw", vec!["\n", "gateway-secret\n"]);

        result.expect("login");
        assert!(
            output.starts_with(&format!("Base URL [{root}]: Using {root}/v1\nAPI key: ")),
            "{output}"
        );
        assert!(output.contains("Updated provider gw in "), "{output}");
        assert!(!output.contains("Default model:"), "{output}");
        assert_eq!(
            home.providers(),
            json!({"providers": {"gw": {
                "api_family": "openai_chat_completions", "base_url": format!("{root}/v1"), "auth_header": true,
                "headers": {"x-team": "research"}, "default_model": "gpt-4o", "discovery": true
            }}})
        );
        assert_eq!(home.stored_key("gw").as_deref(), Some("gateway-secret"));
    }

    #[test]
    fn custom_provider_login_keeps_an_unlisted_default_model_and_never_picks_a_responses_model() {
        static ROUTES: &[(&str, u16, &str)] = &[("/model/info", 200, MODEL_INFO)];
        let (root, _requests) = spawn_gateway(ROUTES);
        let unlisted = LoginHome::new(Some(leak(format!(
            r#"{{"providers": {{"gw": {{"api_family": "openai_chat_completions", "base_url": "{root}/v1", "auth_header": true, "discovery": true, "default_model": "retired-model"}}}}}}"#
        ))));
        // The configured entry moves claude-sonnet to Responses, and gpt-4o
        // declares no tool support, so no model qualifies.
        let overridden = LoginHome::new(Some(leak(format!(
            r#"{{"providers": {{"gw": {{"api_family": "openai_chat_completions", "base_url": "{root}/v1", "auth_header": true, "models": [{{"id": "claude-sonnet", "api_family": "openai_responses"}}]}}}}}}"#
        ))));

        for home in [&unlisted, &overridden] {
            let (result, output, _) = home.login("gw", vec!["\n", "gateway-secret\n"]);

            result.expect("login");
            assert!(output.contains("Updated provider gw in "), "{output}");
            assert!(!output.contains("Default model:"), "{output}");
        }
        assert_eq!(
            unlisted.providers()["providers"]["gw"]["default_model"],
            "retired-model"
        );
        assert!(overridden.providers()["providers"]["gw"]
            .get("default_model")
            .is_none());
    }

    #[test]
    fn custom_provider_login_stores_keys_starting_with_dollar_or_bang_as_literals() {
        static ROUTES: &[(&str, u16, &str)] = &[("/model/info", 200, MODEL_INFO)];
        let (root, _requests) = spawn_gateway(ROUTES);
        let url = leak(format!("{root}\n"));
        for key in [
            "$gateway-secret",
            "!gateway-secret",
            "${GATEWAY}",
            "$$x",
            "$!x",
            "$",
            "!",
        ] {
            let home = LoginHome::new(None);

            let (result, _, _) = home.login("gw", vec![url, key]);

            result.expect("login");
            assert_eq!(home.stored_key("gw").as_deref(), Some(key), "{key}");
        }
    }

    #[test]
    fn custom_provider_login_refusals_store_nothing() {
        const BEARER_ENTRY: &str = r#"{"providers": {"gw": {"api_family": "openai_chat_completions", "base_url": "https://gateway.example.com/v1", "auth_header": true, "default_model": "m"}}}"#;
        let cases: [(&str, &str, Vec<&'static str>, &str); 12] = [
            (
                BEARER_ENTRY,
                "../../escaped",
                vec![],
                "provider id `../../escaped` is not valid: custom provider ids must be lowercase ASCII letters, numbers, '-' or '_'; nothing was stored",
            ),
            (
                BEARER_ENTRY,
                "a b",
                vec![],
                "provider id `a b` is not valid: custom provider ids must be lowercase ASCII letters, numbers, '-' or '_'; nothing was stored",
            ),
            (
                r#"{"providers": []}"#,
                "gw",
                vec![],
                "providers in PROVIDERS is not an object",
            ),
            (
                r#"{"providers": {"gw": {"api_family": "openai_chat_completions", "base_url": "https://gateway.example.com/v1", "auth_header": true, "api_key": "$GW_KEY", "default_model": "m"}}}"#,
                "gw",
                vec![],
                "provider gw sets api_key in PROVIDERS; remove api_key to use a stored key",
            ),
            (
                r#"{"providers": {"gw": {"api_family": "openai_chat_completions", "base_url": "https://gateway.example.com/v1", "default_model": "m"}}}"#,
                "gw",
                vec![],
                "provider gw does not set auth_header: true in PROVIDERS, so a stored key would not be sent; set auth_header: true",
            ),
            (
                r#"{"providers": {"gw": {"api_family": "openai_chat_completions", "base_url": "https://gateway.example.com/proxy", "auth_header": true, "discovery": true}}}"#,
                "gw",
                vec![],
                "provider gw in PROVIDERS is not valid (see the warning above); fix or remove it, then run euler login --provider gw",
            ),
            (
                r#"{"providers": {"#,
                "gw",
                vec![],
                "PROVIDERS is not valid JSON (EOF while parsing an object at line 1 column 15); fix it, then run euler login again",
            ),
            (BEARER_ENTRY, "new", vec!["\n"], "no base URL entered; nothing was stored"),
            (
                BEARER_ENTRY,
                "new",
                vec!["https://gateway.example.com/v1/chat/completions\n"],
                "the base URL ends in the endpoint path /chat/completions; enter the gateway's base URL without it",
            ),
            (
                BEARER_ENTRY,
                "new",
                vec!["http://gateway.example.com\n"],
                "the base URL uses HTTP to a non-loopback host, so the key would travel unencrypted; use https://gateway.example.com/v1",
            ),
            (
                BEARER_ENTRY,
                "new",
                vec!["https://gateway.example.com\n", "  \n"],
                "no API key entered; nothing was stored",
            ),
            (
                BEARER_ENTRY,
                "gw",
                vec!["\n", "  \n"],
                "no API key entered; nothing was stored",
            ),
        ];
        for (providers_json, provider_id, input, expected) in cases {
            let home = LoginHome::new(Some(providers_json));

            let (result, output, _) = home.login(provider_id, input);

            let expected =
                expected.replace("PROVIDERS", &home.providers_path.display().to_string());
            if expected.starts_with("provider id ") {
                assert_eq!(output, "", "the id is refused before any prompt");
            }
            assert_eq!(result.expect_err("refused").to_string(), expected);
            assert!(!home.auth_path.exists(), "{expected}");
            assert_eq!(
                fs::read_to_string(&home.providers_path).expect("providers.json"),
                providers_json
            );
        }
    }

    #[test]
    fn failed_custom_provider_login_for_a_new_id_names_the_error_and_writes_nothing() {
        static REJECTED: &[(&str, u16, &str)] =
            &[("/model/info", 401, "{}"), ("/v1/models", 401, "{}")];
        static NO_MODELS: &[(&str, u16, &str)] = &[];
        for (routes, expected) in [
            (REJECTED, "could not verify provider gw: the key was rejected: GET ROOT/v1/models returned HTTP 401 (GET ROOT/model/info returned HTTP 401); nothing was stored"),
            (NO_MODELS, "could not verify provider gw: GET ROOT/v1/models returned HTTP 404 (GET ROOT/model/info returned HTTP 404); nothing was stored"),
        ] {
            let (root, _requests) = spawn_gateway(routes);
            let home = LoginHome::new(None);

            let (result, _, _) = home.login("gw", vec![leak(format!("{root}\n")), "gateway-secret\n"]);

            assert_eq!(
                result.expect_err("failed").to_string(),
                expected.replace("ROOT", &root)
            );
            assert!(!home.auth_path.exists());
            assert!(!home.providers_path.exists());
            assert!(!home.cache_path("gw").exists());
        }
    }

    #[test]
    fn existing_entry_whose_gateway_lists_no_models_can_store_the_key_with_the_entry_unchanged() {
        static NO_MODELS: &[(&str, u16, &str)] = &[];
        static REJECTED: &[(&str, u16, &str)] =
            &[("/model/info", 403, "{}"), ("/v1/models", 403, "{}")];
        let entry_for = |root: &str| {
            leak(format!(
                r#"{{"providers": {{"gw": {{"api_family": "openai_chat_completions", "base_url": "{root}/v1", "auth_header": true, "default_model": "m"}}}}}}"#
            ))
        };
        let (root, _requests) = spawn_gateway(NO_MODELS);
        let entry = entry_for(&root);

        let home = LoginHome::new(Some(entry));
        let (result, output, _) = home.login("gw", vec!["\n", "gateway-secret\n", "\n"]);
        result.expect("saved without discovery");
        assert!(
            output.ends_with(&format!(
                "API key: Model discovery failed: GET {root}/v1/models returned HTTP 404 (GET {root}/model/info returned HTTP 404).\nSave the key without model discovery? [Y/n] Stored the key for gw in {}. Its providers.json entry is unchanged, so the key is sent to {root}/v1.\n",
                home.auth_path.display()
            )),
            "{output}"
        );
        assert_eq!(home.stored_key("gw").as_deref(), Some("gateway-secret"));
        assert_eq!(
            fs::read_to_string(&home.providers_path).expect("providers"),
            entry
        );

        let home = LoginHome::new(Some(entry));
        let (result, _, _) = home.login("gw", vec!["\n", "gateway-secret\n", "n\n"]);
        assert_eq!(
            result.expect_err("declined").to_string(),
            "login cancelled; nothing was stored"
        );
        assert!(!home.auth_path.exists());

        // A changed base URL is not offered the save, since the entry would
        // keep the old one.
        let (other_root, _other_requests) = spawn_gateway(NO_MODELS);
        let home = LoginHome::new(Some(entry));
        let (result, output, _) = home.login(
            "gw",
            vec![leak(format!("{other_root}\n")), "gateway-secret\n", "y\n"],
        );
        let error = result.expect_err("changed URL").to_string();
        assert!(
            error.starts_with(&format!(
                "could not verify provider gw: GET {other_root}/v1/models returned HTTP 404"
            )),
            "{error}"
        );
        assert!(!output.contains("Save the key"), "{output}");
        assert!(!home.auth_path.exists());
        assert_eq!(
            fs::read_to_string(&home.providers_path).expect("providers"),
            entry
        );

        // Enter keeps an entry's URL that lacks /v1, since it normalizes to
        // the same URL.
        let home = LoginHome::new(Some(leak(entry.replace("/v1\"", "\""))));
        let (result, _, _) = home.login("gw", vec!["\n", "gateway-secret\n", "\n"]);
        result.expect("kept URL without /v1 saved without discovery");
        assert_eq!(home.stored_key("gw").as_deref(), Some("gateway-secret"));

        // Typing the entry's URL in another form counts as keeping it.
        let home = LoginHome::new(Some(entry));
        let (result, _, _) = home.login(
            "gw",
            vec![leak(format!("{root}/\n")), "gateway-secret\n", "\n"],
        );
        result.expect("same URL saved without discovery");
        assert_eq!(home.stored_key("gw").as_deref(), Some("gateway-secret"));

        let (root, _requests) = spawn_gateway(REJECTED);
        let home = LoginHome::new(Some(entry_for(&root)));
        let (result, output, _) = home.login("gw", vec!["\n", "gateway-secret\n", "y\n"]);
        let error = result.expect_err("rejected").to_string();
        assert!(
            error.starts_with("could not verify provider gw: the key was rejected: "),
            "{error}"
        );
        assert!(!output.contains("Save the key"), "{output}");
        assert!(!home.auth_path.exists());
    }

    #[test]
    fn failed_entry_write_restores_the_previous_key_and_failed_cache_write_warns() {
        static ROUTES: &[(&str, u16, &str)] = &[("/model/info", 200, MODEL_INFO)];
        let (root, _requests) = spawn_gateway(ROUTES);
        let url = leak(format!("{root}\n"));

        // A cache directory that cannot be created only warns.
        let home = LoginHome::new(None);
        fs::write(home.providers_path.with_file_name("cache"), "").expect("cache file");
        let (result, output, warnings) = home.login("gw", vec![url, "gateway-secret\n"]);
        result.expect("login");
        assert!(output.contains("Added provider gw to "), "{output}");
        assert!(
            warnings.starts_with("warning: could not create "),
            "{warnings}"
        );
        assert!(
            warnings.ends_with("; the next launch fetches the models\n"),
            "{warnings}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Root ignores directory permissions, so the write would succeed.
            if unsafe { libc::geteuid() } == 0 {
                return;
            }
            let temp = tempfile::tempdir().expect("temp dir");
            let config_dir = temp.path().join("config");
            fs::create_dir(&config_dir).expect("config dir");
            let providers_path = config_dir.join("providers.json");
            fs::write(&providers_path, "{}").expect("providers.json");
            let auth_path = temp.path().join("auth.json");
            store_api_key(&auth_path, "gw", "previous-secret").expect("previous key");
            fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o500)).expect("chmod");

            let result = run_custom_provider_login(
                &CustomProviderLogin {
                    provider_id: "gw",
                    providers: &ProviderConfigRegistry::default(),
                    provider_config: &providers_path,
                    auth_path: &auth_path,
                },
                &mut ScriptedInput(vec![url, "new-secret\n"]),
                Vec::new(),
                Vec::new(),
            );
            fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).expect("chmod");

            let error = result.expect_err("entry write fails").to_string();
            assert!(
                error.starts_with(&format!("could not write {}: ", providers_path.display())),
                "{error}"
            );
            assert!(
                error.ends_with("; the key for gw was not stored"),
                "{error}"
            );
            let restored = AuthStorage::new(&auth_path)
                .expect("storage")
                .resolve_api_key("gw", None)
                .map(|key| key.expose_secret().to_owned());
            assert_eq!(restored.as_deref(), Some("previous-secret"));
            assert_eq!(
                fs::read_to_string(&providers_path).expect("providers"),
                "{}"
            );
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
