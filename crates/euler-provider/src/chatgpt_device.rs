use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::auth::SecretString;
use crate::ProviderError;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEVICE_CODE_TIMEOUT_SECONDS: u64 = 15 * 60;
const SLOW_DOWN_INCREMENT_SECONDS: u64 = 5;
const MAX_TRANSIENT_FAILURES: u8 = 3;
const ACCOUNT_CLAIM_PATH: &str = "https://api.openai.com/auth";

#[derive(Clone, Eq, PartialEq)]
pub struct ChatGptDeviceCode {
    device_auth_id: SecretString,
    user_code: SecretString,
    interval_seconds: u64,
    expires_in_seconds: u64,
    verification_url: &'static str,
}

impl ChatGptDeviceCode {
    pub fn user_code(&self) -> &str {
        self.user_code.expose()
    }

    pub fn verification_url(&self) -> &str {
        self.verification_url
    }

    pub fn expires_in_seconds(&self) -> u64 {
        self.expires_in_seconds
    }
}

impl fmt::Debug for ChatGptDeviceCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatGptDeviceCode")
            .field("device_auth_id", &self.device_auth_id)
            .field("user_code", &self.user_code)
            .field("interval_seconds", &self.interval_seconds)
            .field("expires_in_seconds", &self.expires_in_seconds)
            .field("verification_url", &self.verification_url)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ChatGptLoginCredential {
    pub access: SecretString,
    pub refresh: SecretString,
    pub expires: u64,
    pub account_id: Option<String>,
}

impl ChatGptLoginCredential {
    pub fn into_storage_parts(self) -> ChatGptLoginStorageParts {
        ChatGptLoginStorageParts {
            access: self.access.into_string(),
            refresh: self.refresh.into_string(),
            expires: self.expires,
            account_id: self.account_id,
        }
    }
}

pub struct ChatGptLoginStorageParts {
    pub access: String,
    pub refresh: String,
    pub expires: u64,
    pub account_id: Option<String>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ChatGptRefreshCredential {
    pub access: SecretString,
    pub refresh: Option<SecretString>,
    pub expires: u64,
    pub account_id: Option<String>,
}

impl ChatGptRefreshCredential {
    pub fn into_storage_parts(self) -> ChatGptRefreshStorageParts {
        ChatGptRefreshStorageParts {
            access: self.access.into_string(),
            refresh: self.refresh.map(SecretString::into_string),
            expires: self.expires,
            account_id: self.account_id,
        }
    }
}

pub struct ChatGptRefreshStorageParts {
    pub access: String,
    pub refresh: Option<String>,
    pub expires: u64,
    pub account_id: Option<String>,
}

impl fmt::Debug for ChatGptLoginCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatGptLoginCredential")
            .field("access", &self.access)
            .field("refresh", &self.refresh)
            .field("expires", &self.expires)
            .field("account_id", &self.account_id)
            .finish()
    }
}

impl fmt::Debug for ChatGptRefreshCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatGptRefreshCredential")
            .field("access", &self.access)
            .field("refresh", &self.refresh)
            .field("expires", &self.expires)
            .field("account_id", &self.account_id)
            .finish()
    }
}

pub struct ChatGptDeviceLogin {
    endpoints: DeviceLoginEndpoints,
    http: Box<dyn DeviceLoginHttp>,
    sleeper: Box<dyn DeviceLoginSleeper>,
    clock: Box<dyn DeviceLoginClock>,
}

impl ChatGptDeviceLogin {
    pub fn new() -> Self {
        Self {
            endpoints: DeviceLoginEndpoints::default(),
            http: Box::new(UreqDeviceLoginHttp::new()),
            sleeper: Box::new(ThreadSleeper),
            clock: Box::new(SystemClock),
        }
    }

    pub fn start(&self) -> Result<ChatGptDeviceCode, ProviderError> {
        start_device_auth(self.http.as_ref(), &self.endpoints)
    }

    pub fn finish(
        &self,
        device: &ChatGptDeviceCode,
    ) -> Result<ChatGptLoginCredential, ProviderError> {
        let authorization = poll_device_auth(
            device,
            self.http.as_ref(),
            self.sleeper.as_ref(),
            self.clock.as_ref(),
            &self.endpoints,
        )?;
        exchange_authorization_code(
            &authorization,
            self.http.as_ref(),
            self.clock.as_ref(),
            &self.endpoints,
        )
    }
}

impl Default for ChatGptDeviceLogin {
    fn default() -> Self {
        Self::new()
    }
}

pub fn refresh_chatgpt_oauth(
    refresh_token: &str,
) -> Result<ChatGptRefreshCredential, ProviderError> {
    refresh_oauth_token(
        refresh_token,
        &UreqDeviceLoginHttp::new(),
        &SystemClock,
        &DeviceLoginEndpoints::default(),
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DeviceLoginEndpoints {
    user_code_url: String,
    device_token_url: String,
    oauth_token_url: String,
    verification_url: &'static str,
}

impl Default for DeviceLoginEndpoints {
    fn default() -> Self {
        Self {
            user_code_url: DEVICE_USER_CODE_URL.to_owned(),
            device_token_url: DEVICE_TOKEN_URL.to_owned(),
            oauth_token_url: OAUTH_TOKEN_URL.to_owned(),
            verification_url: DEVICE_VERIFICATION_URL,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HttpResponse {
    status: u16,
    body: String,
}

trait DeviceLoginHttp {
    fn post_json(&self, url: &str, body: Value) -> Result<HttpResponse, ProviderError>;
    fn post_form(&self, url: &str, fields: &[(&str, &str)]) -> Result<HttpResponse, ProviderError>;
}

trait DeviceLoginSleeper {
    fn sleep(&self, duration: Duration);
}

trait DeviceLoginClock {
    fn now_ms(&self) -> u64;
}

struct UreqDeviceLoginHttp {
    agent: ureq::Agent,
}

impl UreqDeviceLoginHttp {
    fn new() -> Self {
        Self {
            agent: ureq::builder().redirects(0).build(),
        }
    }
}

impl DeviceLoginHttp for UreqDeviceLoginHttp {
    fn post_json(&self, url: &str, body: Value) -> Result<HttpResponse, ProviderError> {
        response_from_ureq(
            self.agent
                .post(url)
                .set("Content-Type", "application/json")
                .send_json(body),
        )
    }

    fn post_form(&self, url: &str, fields: &[(&str, &str)]) -> Result<HttpResponse, ProviderError> {
        response_from_ureq(
            self.agent
                .post(url)
                .set("Content-Type", "application/x-www-form-urlencoded")
                .send_form(fields),
        )
    }
}

struct ThreadSleeper;

impl DeviceLoginSleeper for ThreadSleeper {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

struct SystemClock;

impl DeviceLoginClock for SystemClock {
    fn now_ms(&self) -> u64 {
        now_unix_ms()
    }
}

fn response_from_ureq(
    response: Result<ureq::Response, ureq::Error>,
) -> Result<HttpResponse, ProviderError> {
    match response {
        Ok(response) => response_body(response.status(), response),
        Err(ureq::Error::Status(status, response)) => response_body(status, response),
        Err(_) => Err(ProviderError::transport(
            "ChatGPT device login request failed",
        )),
    }
}

fn response_body(status: u16, response: ureq::Response) -> Result<HttpResponse, ProviderError> {
    let body = response
        .into_string()
        .map_err(|_| ProviderError::transport("ChatGPT device login response could not be read"))?;
    Ok(HttpResponse { status, body })
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: String,
    user_code: String,
    interval: IntervalSeconds,
    expires_in: Option<IntervalSeconds>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum IntervalSeconds {
    Number(u64),
    String(String),
}

impl IntervalSeconds {
    fn into_u64(self) -> Option<u64> {
        match self {
            Self::Number(value) => Some(value),
            Self::String(value) => value.trim().parse().ok(),
        }
    }
}

fn start_device_auth(
    http: &dyn DeviceLoginHttp,
    endpoints: &DeviceLoginEndpoints,
) -> Result<ChatGptDeviceCode, ProviderError> {
    let response = http.post_json(&endpoints.user_code_url, json!({ "client_id": CLIENT_ID }))?;
    if response.status != 200 {
        return Err(ProviderError::auth(format!(
            "ChatGPT device-code request failed with HTTP {}",
            response.status
        )));
    }

    let response: DeviceCodeResponse = serde_json::from_str(&response.body)
        .map_err(|_| ProviderError::auth("ChatGPT device-code response was not valid JSON"))?;
    let interval_seconds = response
        .interval
        .into_u64()
        .ok_or_else(|| ProviderError::auth("ChatGPT device-code response had invalid interval"))?;
    let expires_in_seconds = response
        .expires_in
        .and_then(IntervalSeconds::into_u64)
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEVICE_CODE_TIMEOUT_SECONDS);
    if response.device_auth_id.is_empty() || response.user_code.is_empty() {
        return Err(ProviderError::auth(
            "ChatGPT device-code response was missing required fields",
        ));
    }

    Ok(ChatGptDeviceCode {
        device_auth_id: SecretString::new(response.device_auth_id),
        user_code: SecretString::new(response.user_code),
        interval_seconds,
        expires_in_seconds,
        verification_url: endpoints.verification_url,
    })
}

#[derive(Clone, Eq, PartialEq)]
struct DeviceAuthorization {
    authorization_code: SecretString,
    code_verifier: SecretString,
}

impl fmt::Debug for DeviceAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceAuthorization")
            .field("authorization_code", &self.authorization_code)
            .field("code_verifier", &self.code_verifier)
            .finish()
    }
}

#[derive(Deserialize)]
struct DeviceTokenSuccess {
    authorization_code: String,
    code_verifier: String,
}

enum PollOutcome {
    Pending,
    SlowDown,
    Complete(DeviceAuthorization),
    Terminal(ProviderError),
    Transient,
}

fn poll_device_auth(
    device: &ChatGptDeviceCode,
    http: &dyn DeviceLoginHttp,
    sleeper: &dyn DeviceLoginSleeper,
    clock: &dyn DeviceLoginClock,
    endpoints: &DeviceLoginEndpoints,
) -> Result<DeviceAuthorization, ProviderError> {
    let deadline = clock
        .now_ms()
        .saturating_add(device.expires_in_seconds.saturating_mul(1000));
    let mut interval = device.interval_seconds.max(1);
    let mut transient_failures = 0u8;

    while clock.now_ms() < deadline {
        let outcome = poll_device_once(device, http, endpoints)?;
        match outcome {
            PollOutcome::Complete(authorization) => return Ok(authorization),
            PollOutcome::Pending => transient_failures = 0,
            PollOutcome::SlowDown => {
                transient_failures = 0;
                interval = interval.saturating_add(SLOW_DOWN_INCREMENT_SECONDS);
            }
            PollOutcome::Terminal(error) => return Err(error),
            PollOutcome::Transient => {
                transient_failures = transient_failures.saturating_add(1);
                if transient_failures > MAX_TRANSIENT_FAILURES {
                    return Err(ProviderError::transport(
                        "ChatGPT device authorization polling failed repeatedly",
                    ));
                }
            }
        }

        let remaining_ms = deadline.saturating_sub(clock.now_ms());
        if remaining_ms == 0 {
            break;
        }
        let sleep_ms = remaining_ms.min(interval.saturating_mul(1000));
        sleeper.sleep(Duration::from_millis(sleep_ms));
    }

    Err(ProviderError::auth("ChatGPT device authorization expired"))
}

fn poll_device_once(
    device: &ChatGptDeviceCode,
    http: &dyn DeviceLoginHttp,
    endpoints: &DeviceLoginEndpoints,
) -> Result<PollOutcome, ProviderError> {
    let response = match http.post_json(
        &endpoints.device_token_url,
        json!({
            "device_auth_id": device.device_auth_id.expose(),
            "user_code": device.user_code.expose(),
        }),
    ) {
        Ok(response) => response,
        Err(_) => return Ok(PollOutcome::Transient),
    };

    if response.status == 200 {
        let parsed: DeviceTokenSuccess = serde_json::from_str(&response.body).map_err(|_| {
            ProviderError::auth("ChatGPT device authorization response was not valid JSON")
        })?;
        if parsed.authorization_code.is_empty() || parsed.code_verifier.is_empty() {
            return Ok(PollOutcome::Terminal(ProviderError::auth(
                "ChatGPT device authorization response was missing required fields",
            )));
        }
        return Ok(PollOutcome::Complete(DeviceAuthorization {
            authorization_code: SecretString::new(parsed.authorization_code),
            code_verifier: SecretString::new(parsed.code_verifier),
        }));
    }

    if response.status == 403 || response.status == 404 {
        return Ok(PollOutcome::Pending);
    }

    if (500..=599).contains(&response.status) {
        return Ok(PollOutcome::Transient);
    }

    match provider_error_code(&response.body).as_deref() {
        Some("deviceauth_authorization_pending") => Ok(PollOutcome::Pending),
        Some("authorization_pending") => Ok(PollOutcome::Pending),
        Some("slow_down") => Ok(PollOutcome::SlowDown),
        Some("expired_token") => Ok(PollOutcome::Terminal(ProviderError::auth(
            "ChatGPT device authorization expired",
        ))),
        Some("access_denied") => Ok(PollOutcome::Terminal(ProviderError::auth(
            "ChatGPT device authorization was denied",
        ))),
        _ => Ok(PollOutcome::Terminal(ProviderError::auth(format!(
            "ChatGPT device authorization failed with HTTP {}",
            response.status
        )))),
    }
}

fn provider_error_code(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    match value.get("error")? {
        Value::String(error) => Some(error.clone()),
        Value::Object(error) => error.get("code")?.as_str().map(str::to_owned),
        _ => None,
    }
}

#[derive(Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

fn exchange_authorization_code(
    authorization: &DeviceAuthorization,
    http: &dyn DeviceLoginHttp,
    clock: &dyn DeviceLoginClock,
    endpoints: &DeviceLoginEndpoints,
) -> Result<ChatGptLoginCredential, ProviderError> {
    let fields = [
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", authorization.authorization_code.expose()),
        ("code_verifier", authorization.code_verifier.expose()),
        ("redirect_uri", DEVICE_REDIRECT_URI),
    ];
    let response = http.post_form(&endpoints.oauth_token_url, &fields)?;
    if response.status != 200 {
        return Err(ProviderError::auth(format!(
            "ChatGPT OAuth token exchange failed with HTTP {}",
            response.status
        )));
    }

    let parsed: OAuthTokenResponse = serde_json::from_str(&response.body)
        .map_err(|_| ProviderError::auth("ChatGPT OAuth token response was not valid JSON"))?;
    let Some(refresh_token) = parsed.refresh_token.filter(|token| !token.is_empty()) else {
        return Err(ProviderError::auth(
            "ChatGPT OAuth token response was missing required fields",
        ));
    };
    if parsed.access_token.is_empty() {
        return Err(ProviderError::auth(
            "ChatGPT OAuth token response was missing required fields",
        ));
    }
    let expires = clock
        .now_ms()
        .saturating_add(parsed.expires_in.saturating_mul(1000));
    let account_id = account_id_from_access_token(&parsed.access_token);

    Ok(ChatGptLoginCredential {
        access: SecretString::new(parsed.access_token),
        refresh: SecretString::new(refresh_token),
        expires,
        account_id,
    })
}

fn refresh_oauth_token(
    refresh_token: &str,
    http: &dyn DeviceLoginHttp,
    clock: &dyn DeviceLoginClock,
    endpoints: &DeviceLoginEndpoints,
) -> Result<ChatGptRefreshCredential, ProviderError> {
    let fields = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
    ];
    let response = http.post_form(&endpoints.oauth_token_url, &fields)?;
    if response.status != 200 {
        return Err(ProviderError::auth(format!(
            "ChatGPT OAuth refresh failed with HTTP {}",
            response.status
        )));
    }

    let parsed: OAuthTokenResponse = serde_json::from_str(&response.body)
        .map_err(|_| ProviderError::auth("ChatGPT OAuth refresh response was not valid JSON"))?;
    if parsed.access_token.is_empty() {
        return Err(ProviderError::auth(
            "ChatGPT OAuth refresh response was missing required fields",
        ));
    }
    let expires = clock
        .now_ms()
        .saturating_add(parsed.expires_in.saturating_mul(1000));
    let account_id = account_id_from_access_token(&parsed.access_token);

    Ok(ChatGptRefreshCredential {
        access: SecretString::new(parsed.access_token),
        refresh: parsed
            .refresh_token
            .filter(|token| !token.is_empty())
            .map(SecretString::new),
        expires,
        account_id,
    })
}

fn account_id_from_access_token(access_token: &str) -> Option<String> {
    let payload = access_token.split('.').nth(1)?;
    let decoded = base64_url_decode(payload)?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    value
        .get(ACCOUNT_CLAIM_PATH)?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|account_id| !account_id.is_empty())
        .map(str::to_owned)
}

fn base64_url_decode(input: &str) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u8;

    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(((buffer >> bits) & 0xff) as u8);
            buffer &= (1 << bits) - 1;
        }
    }

    Some(output)
}

fn now_unix_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    millis.min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
#[path = "chatgpt_device_tests.rs"]
mod tests;
