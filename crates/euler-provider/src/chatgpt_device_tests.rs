use super::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

#[test]
fn chatgpt_device_start_sends_expected_request_and_parses_response() {
    let http = FakeHttp::new([HttpResponse {
        status: 200,
        body: json!({
            "device_auth_id": "device-auth-id",
            "user_code": "USER-CODE",
            "interval": 2,
            "expires_in": 1200,
        })
        .to_string(),
    }]);

    let device = start_device_auth(&http, &DeviceLoginEndpoints::default()).expect("start");

    assert_eq!(device.user_code(), "USER-CODE");
    assert_eq!(device.verification_url(), DEVICE_VERIFICATION_URL);
    assert_eq!(device.expires_in_seconds(), 1200);
    assert_eq!(
        http.requests(),
        vec![RecordedRequest::Json {
            url: DEVICE_USER_CODE_URL.to_owned(),
            body: json!({ "client_id": CLIENT_ID }),
        }]
    );
    let formatted = format!("{device:?}");
    assert!(!formatted.contains("device-auth-id"));
    assert!(!formatted.contains("USER-CODE"));
}

#[test]
fn chatgpt_device_start_falls_back_to_default_expiry_when_server_omits_it() {
    let http = FakeHttp::new([HttpResponse {
        status: 200,
        body: json!({
            "device_auth_id": "device-auth-id",
            "user_code": "USER-CODE",
            "interval": 2,
        })
        .to_string(),
    }]);

    let device = start_device_auth(&http, &DeviceLoginEndpoints::default()).expect("start");

    assert_eq!(device.expires_in_seconds(), DEVICE_CODE_TIMEOUT_SECONDS);
}

#[test]
fn chatgpt_device_start_falls_back_to_default_expiry_when_server_value_is_invalid() {
    let http = FakeHttp::new([HttpResponse {
        status: 200,
        body: json!({
            "device_auth_id": "device-auth-id",
            "user_code": "USER-CODE",
            "interval": 2,
            "expires_in": "not-seconds",
        })
        .to_string(),
    }]);

    let device = start_device_auth(&http, &DeviceLoginEndpoints::default()).expect("start");

    assert_eq!(device.expires_in_seconds(), DEVICE_CODE_TIMEOUT_SECONDS);
}

#[test]
fn chatgpt_device_poll_handles_pending_slow_down_and_success() {
    let http = FakeHttp::new([
        HttpResponse {
            status: 403,
            body: String::new(),
        },
        HttpResponse {
            status: 400,
            body: json!({ "error": "slow_down" }).to_string(),
        },
        HttpResponse {
            status: 200,
            body: json!({
                "authorization_code": "authorization-secret",
                "code_verifier": "verifier-secret",
            })
            .to_string(),
        },
    ]);
    let clock_value = Rc::new(Cell::new(1_000));
    let clock = TestClock {
        now: Rc::clone(&clock_value),
    };
    let sleeper = AdvancingSleeper {
        now: clock_value,
        sleeps: RefCell::new(Vec::new()),
    };
    let device = test_device_code(2);

    let authorization = poll_device_auth(
        &device,
        &http,
        &sleeper,
        &clock,
        &DeviceLoginEndpoints::default(),
    )
    .expect("authorization");

    assert_eq!(
        authorization.authorization_code.expose(),
        "authorization-secret"
    );
    assert_eq!(authorization.code_verifier.expose(), "verifier-secret");
    assert_eq!(
        sleeper.sleeps(),
        vec![Duration::from_secs(2), Duration::from_secs(7)]
    );
    assert_eq!(http.requests().len(), 3);
    assert!(http.requests().iter().all(|request| {
        matches!(
            request,
            RecordedRequest::Json { url, body }
                if url == DEVICE_TOKEN_URL
                    && body["device_auth_id"] == "device-auth-id"
                    && body["user_code"] == "USER-CODE"
        )
    }));
}

#[test]
fn chatgpt_device_poll_retries_transport_errors_then_succeeds() {
    let http = FakeHttp::new([
        QueuedResponse::TransportError,
        HttpResponse {
            status: 403,
            body: String::new(),
        }
        .into(),
        HttpResponse {
            status: 200,
            body: json!({
                "authorization_code": "authorization-secret",
                "code_verifier": "verifier-secret",
            })
            .to_string(),
        }
        .into(),
    ]);
    let clock_value = Rc::new(Cell::new(1_000));
    let clock = TestClock {
        now: Rc::clone(&clock_value),
    };
    let sleeper = AdvancingSleeper {
        now: clock_value,
        sleeps: RefCell::new(Vec::new()),
    };

    let authorization = poll_device_auth(
        &test_device_code(2),
        &http,
        &sleeper,
        &clock,
        &DeviceLoginEndpoints::default(),
    )
    .expect("authorization");

    assert_eq!(
        authorization.authorization_code.expose(),
        "authorization-secret"
    );
    assert_eq!(sleeper.sleeps(), vec![Duration::from_secs(2); 2]);
    assert_eq!(http.requests().len(), 3);
}

#[test]
fn chatgpt_device_poll_fails_after_repeated_transport_errors() {
    let attempts = usize::from(MAX_TRANSIENT_FAILURES) + 1;
    let http = FakeHttp::new(std::iter::repeat_n(
        QueuedResponse::TransportError,
        attempts,
    ));
    let clock_value = Rc::new(Cell::new(1_000));
    let clock = TestClock {
        now: Rc::clone(&clock_value),
    };
    let sleeper = AdvancingSleeper {
        now: clock_value,
        sleeps: RefCell::new(Vec::new()),
    };

    let error = poll_device_auth(
        &test_device_code(2),
        &http,
        &sleeper,
        &clock,
        &DeviceLoginEndpoints::default(),
    )
    .expect_err("persistent transport failure");

    assert_eq!(error.category(), crate::ProviderErrorCategory::Transport);
    assert_eq!(http.requests().len(), attempts);
    assert_eq!(sleeper.sleeps(), vec![Duration::from_secs(2); 3]);
}

#[test]
fn chatgpt_token_exchange_parses_tokens_expiry_and_account_id_without_debug_leak() {
    let access = access_token_with_account("account-123");
    let http = FakeHttp::new([HttpResponse {
        status: 200,
        body: json!({
            "access_token": access,
            "refresh_token": "refresh-secret",
            "expires_in": 3600,
        })
        .to_string(),
    }]);
    let clock = TestClock {
        now: Rc::new(Cell::new(10_000)),
    };
    let authorization = DeviceAuthorization {
        authorization_code: SecretString::new("authorization-secret"),
        code_verifier: SecretString::new("verifier-secret"),
    };

    let credential = exchange_authorization_code(
        &authorization,
        &http,
        &clock,
        &DeviceLoginEndpoints::default(),
    )
    .expect("exchange");

    assert_eq!(credential.access.expose(), access);
    assert_eq!(credential.refresh.expose(), "refresh-secret");
    assert_eq!(credential.expires, 3_610_000);
    assert_eq!(credential.account_id.as_deref(), Some("account-123"));
    assert_eq!(
        http.requests(),
        vec![RecordedRequest::Form {
            url: OAUTH_TOKEN_URL.to_owned(),
            fields: vec![
                ("grant_type".to_owned(), "authorization_code".to_owned()),
                ("client_id".to_owned(), CLIENT_ID.to_owned()),
                ("code".to_owned(), "authorization-secret".to_owned()),
                ("code_verifier".to_owned(), "verifier-secret".to_owned()),
                ("redirect_uri".to_owned(), DEVICE_REDIRECT_URI.to_owned()),
            ],
        }]
    );

    let formatted = format!("{credential:?}");
    assert!(formatted.contains("[redacted]"));
    assert!(!formatted.contains(&access));
    assert!(!formatted.contains("refresh-secret"));
}

#[test]
fn chatgpt_token_exchange_errors_do_not_include_token_values_or_raw_body() {
    let http = FakeHttp::new([HttpResponse {
        status: 400,
        body: json!({
            "access_token": "access-secret",
            "refresh_token": "refresh-secret",
            "error": "invalid_grant",
        })
        .to_string(),
    }]);
    let clock = TestClock {
        now: Rc::new(Cell::new(10_000)),
    };
    let authorization = DeviceAuthorization {
        authorization_code: SecretString::new("authorization-secret"),
        code_verifier: SecretString::new("verifier-secret"),
    };

    let error = exchange_authorization_code(
        &authorization,
        &http,
        &clock,
        &DeviceLoginEndpoints::default(),
    )
    .expect_err("exchange failure")
    .to_string();

    assert!(!error.contains("access-secret"));
    assert!(!error.contains("refresh-secret"));
    assert!(!error.contains("authorization-secret"));
    assert!(!error.contains("verifier-secret"));
}

#[test]
fn chatgpt_refresh_sends_expected_form_and_parses_rotated_token() {
    let access = access_token_with_account("account-rotated");
    let http = FakeHttp::new([HttpResponse {
        status: 200,
        body: json!({
            "access_token": access,
            "refresh_token": "rotated-refresh-secret",
            "expires_in": 1800,
        })
        .to_string(),
    }]);
    let clock = TestClock {
        now: Rc::new(Cell::new(20_000)),
    };

    let credential = refresh_oauth_token(
        "old-refresh-secret",
        &http,
        &clock,
        &DeviceLoginEndpoints::default(),
    )
    .expect("refresh");

    assert_eq!(credential.access.expose(), access);
    assert_eq!(
        credential.refresh.as_ref().map(SecretString::expose),
        Some("rotated-refresh-secret")
    );
    assert_eq!(credential.expires, 1_820_000);
    assert_eq!(credential.account_id.as_deref(), Some("account-rotated"));
    assert_eq!(
        http.requests(),
        vec![RecordedRequest::Form {
            url: OAUTH_TOKEN_URL.to_owned(),
            fields: vec![
                ("grant_type".to_owned(), "refresh_token".to_owned()),
                ("refresh_token".to_owned(), "old-refresh-secret".to_owned()),
                ("client_id".to_owned(), CLIENT_ID.to_owned()),
            ],
        }]
    );

    let formatted = format!("{credential:?}");
    assert!(formatted.contains("[redacted]"));
    assert!(!formatted.contains(&access));
    assert!(!formatted.contains("rotated-refresh-secret"));
}

#[test]
fn chatgpt_refresh_allows_absent_rotation() {
    let http = FakeHttp::new([HttpResponse {
        status: 200,
        body: json!({
            "access_token": "new-access-secret",
            "expires_in": 60,
        })
        .to_string(),
    }]);
    let clock = TestClock {
        now: Rc::new(Cell::new(5_000)),
    };

    let credential = refresh_oauth_token(
        "old-refresh-secret",
        &http,
        &clock,
        &DeviceLoginEndpoints::default(),
    )
    .expect("refresh");

    assert_eq!(credential.access.expose(), "new-access-secret");
    assert!(credential.refresh.is_none());
    assert_eq!(credential.expires, 65_000);
}

#[test]
fn chatgpt_refresh_errors_do_not_include_tokens_or_raw_provider_body() {
    let http = FakeHttp::new([HttpResponse {
        status: 400,
        body: json!({
            "access_token": "access-secret",
            "refresh_token": "refresh-secret",
            "error": "invalid_grant",
        })
        .to_string(),
    }]);
    let clock = TestClock {
        now: Rc::new(Cell::new(10_000)),
    };

    let error = refresh_oauth_token(
        "old-refresh-secret",
        &http,
        &clock,
        &DeviceLoginEndpoints::default(),
    )
    .expect_err("refresh failure")
    .to_string();

    assert!(!error.contains("access-secret"));
    assert!(!error.contains("refresh-secret"));
    assert!(!error.contains("old-refresh-secret"));
    assert!(!error.contains("invalid_grant"));
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RecordedRequest {
    Json {
        url: String,
        body: Value,
    },
    Form {
        url: String,
        fields: Vec<(String, String)>,
    },
}

#[derive(Clone)]
enum QueuedResponse {
    Response(HttpResponse),
    TransportError,
}

impl From<HttpResponse> for QueuedResponse {
    fn from(response: HttpResponse) -> Self {
        Self::Response(response)
    }
}

struct FakeHttp {
    responses: RefCell<VecDeque<QueuedResponse>>,
    requests: RefCell<Vec<RecordedRequest>>,
}

impl FakeHttp {
    fn new<T>(responses: impl IntoIterator<Item = T>) -> Self
    where
        T: Into<QueuedResponse>,
    {
        Self {
            responses: RefCell::new(responses.into_iter().map(Into::into).collect()),
            requests: RefCell::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.borrow().clone()
    }
}

impl DeviceLoginHttp for FakeHttp {
    fn post_json(&self, url: &str, body: Value) -> Result<HttpResponse, ProviderError> {
        self.requests.borrow_mut().push(RecordedRequest::Json {
            url: url.to_owned(),
            body,
        });
        match self.responses.borrow_mut().pop_front() {
            Some(QueuedResponse::Response(response)) => Ok(response),
            Some(QueuedResponse::TransportError) => {
                Err(ProviderError::transport("fixture transport failure"))
            }
            None => Err(ProviderError::transport("fixture response queue exhausted")),
        }
    }

    fn post_form(&self, url: &str, fields: &[(&str, &str)]) -> Result<HttpResponse, ProviderError> {
        self.requests.borrow_mut().push(RecordedRequest::Form {
            url: url.to_owned(),
            fields: fields
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        });
        match self.responses.borrow_mut().pop_front() {
            Some(QueuedResponse::Response(response)) => Ok(response),
            Some(QueuedResponse::TransportError) => {
                Err(ProviderError::transport("fixture transport failure"))
            }
            None => Err(ProviderError::transport("fixture response queue exhausted")),
        }
    }
}

struct TestClock {
    now: Rc<Cell<u64>>,
}

impl DeviceLoginClock for TestClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }
}

struct AdvancingSleeper {
    now: Rc<Cell<u64>>,
    sleeps: RefCell<Vec<Duration>>,
}

impl AdvancingSleeper {
    fn sleeps(&self) -> Vec<Duration> {
        self.sleeps.borrow().clone()
    }
}

impl DeviceLoginSleeper for AdvancingSleeper {
    fn sleep(&self, duration: Duration) {
        self.sleeps.borrow_mut().push(duration);
        self.now.set(
            self.now
                .get()
                .saturating_add(duration.as_millis().min(u128::from(u64::MAX)) as u64),
        );
    }
}

fn test_device_code(interval_seconds: u64) -> ChatGptDeviceCode {
    ChatGptDeviceCode {
        device_auth_id: SecretString::new("device-auth-id"),
        user_code: SecretString::new("USER-CODE"),
        interval_seconds,
        expires_in_seconds: DEVICE_CODE_TIMEOUT_SECONDS,
        verification_url: DEVICE_VERIFICATION_URL,
    }
}

fn access_token_with_account(account_id: &str) -> String {
    let payload = json!({
        ACCOUNT_CLAIM_PATH: {
            "chatgpt_account_id": account_id,
        }
    });
    format!(
        "header.{}.signature",
        base64_url_encode(payload.to_string().as_bytes())
    )
}

fn base64_url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::new();
    let mut index = 0;
    while index < bytes.len() {
        let b0 = bytes[index];
        let b1 = bytes.get(index + 1).copied();
        let b2 = bytes.get(index + 2).copied();

        output.push(ALPHABET[(b0 >> 2) as usize] as char);
        output
            .push(ALPHABET[(((b0 & 0b0000_0011) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        if let Some(b1) = b1 {
            output.push(
                ALPHABET[(((b1 & 0b0000_1111) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char,
            );
        }
        if let Some(b2) = b2 {
            output.push(ALPHABET[(b2 & 0b0011_1111) as usize] as char);
        }
        index += 3;
    }
    output
}
