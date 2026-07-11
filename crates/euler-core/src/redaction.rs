//! Secret redaction for tool output (issue #56).
//!
//! Two layers, applied at the tool-result emission chokepoint so redacted
//! text is what reaches BOTH the model canvas and the durable ledger:
//!
//! 1. **Known values**: secrets euler itself can enumerate — configured
//!    secret env vars at session start, plus values the host adds at runtime
//!    (auth-file credentials, resolved `x-secret` values). Exact substring
//!    replacement.
//! 2. **Known token shapes**: well-known credential prefixes (`sk-or-v1-`,
//!    `sk-ant-`, `ghp_`, `AKIA…`, …) caught even when euler cannot know the
//!    value — e.g. a shell command reading a foreign secrets file. This is a
//!    heuristic, not a guarantee; over-matching costs a masked token, which
//!    is the safe direction.
//!
//! The incident this guards against: a session-granted `cat` read a local
//! secret store; the raw key persisted to provenance and travelled to the
//! provider inside the next model call's context.

const REDACTED: &str = "[redacted-secret]";
/// Values shorter than this are ignored: exact-matching tiny strings would
/// mangle ordinary output far more often than it would protect anything.
const MIN_VALUE_LEN: usize = 8;
/// Minimum run length after a known prefix before it reads as a credential.
const MIN_TOKEN_TAIL: usize = 12;

/// Environment variables whose values are secret-tainted when present.
/// Mirrors the env-scrub list in `tools.rs` — both lists must grow together.
const SECRET_ENV_NAMES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    "XAI_API_KEY",
];

/// Credential prefixes redacted by shape. Each entry is (prefix, charset).
const TOKEN_PREFIXES: &[&str] = &[
    "sk-or-v1-",
    "sk-ant-",
    "sk-proj-",
    "ghp_",
    "github_pat_",
    "xai-",
    "xoxb-",
    "xoxp-",
    "AKIA",
    "AIza",
];

#[derive(Clone, Debug, Default)]
pub struct SecretRedactor {
    values: Vec<String>,
}

impl SecretRedactor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed from the configured secret environment variables.
    pub fn from_env() -> Self {
        let mut redactor = Self::new();
        for name in SECRET_ENV_NAMES {
            if let Ok(value) = std::env::var(name) {
                redactor.add_value(value);
            }
        }
        redactor
    }

    /// Add a known secret value (auth-file credential, resolved `x-secret`).
    /// Short values are ignored — see `MIN_VALUE_LEN`.
    pub fn add_value(&mut self, value: impl Into<String>) {
        let value = value.into();
        if value.len() >= MIN_VALUE_LEN && !self.values.contains(&value) {
            self.values.push(value);
        }
    }

    /// Redact string-valued fields of a payload in place (diff / old / new
    /// content fields on patch and file-diff events — the secrets contract
    /// covers ALL provenance payloads, not just tool output).
    pub fn redact_payload_fields(&self, payload: &mut euler_event::JsonObject, fields: &[&str]) {
        for field in fields {
            if let Some(value) = payload.get_mut(*field) {
                if let Some(text) = value.as_str() {
                    let redacted = self.redact(text);
                    if redacted != text {
                        *value = redacted.into();
                    }
                }
            }
        }
    }

    /// Redact every string leaf of a JSON value in place. Tool-call inputs
    /// are arbitrary provider-authored JSON (`{command}`, `{path, content}`,
    /// a patch envelope) persisted to the ledger and replayed into later
    /// model calls via the canvas — a secret the model echoes into an input
    /// must not survive there any more than one in a tool result (secrets
    /// contract, "provenance payloads"). Object keys are left untouched;
    /// only values are redacted.
    pub fn redact_value(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => {
                let redacted = self.redact(text);
                if &redacted != text {
                    *text = redacted;
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    self.redact_value(item);
                }
            }
            serde_json::Value::Object(map) => {
                for (_key, val) in map.iter_mut() {
                    self.redact_value(val);
                }
            }
            _ => {}
        }
    }

    /// Redact known values and known token shapes from `text`.
    pub fn redact(&self, text: &str) -> String {
        let mut out = self
            .values
            .iter()
            .fold(text.to_owned(), |acc, value| acc.replace(value, REDACTED));
        out = redact_token_shapes(&out);
        out
    }
}

fn token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'
}

/// Replace `prefix + [A-Za-z0-9_-]{MIN_TOKEN_TAIL,}` runs with the marker.
fn redact_token_shapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    'outer: while !rest.is_empty() {
        for prefix in TOKEN_PREFIXES {
            if let Some(pos) = rest.find(prefix) {
                // Take the earliest prefix hit across all prefixes.
                let earliest = TOKEN_PREFIXES
                    .iter()
                    .filter_map(|p| rest.find(p).map(|at| (at, *p)))
                    .min_by_key(|(at, _)| *at);
                let Some((at, hit)) = earliest else { break };
                let _ = (pos, prefix);
                let tail_start = at + hit.len();
                let tail_len = rest[tail_start..]
                    .chars()
                    .take_while(|ch| token_char(*ch))
                    .map(char::len_utf8)
                    .sum::<usize>();
                out.push_str(&rest[..at]);
                if tail_len >= MIN_TOKEN_TAIL {
                    out.push_str(REDACTED);
                } else {
                    out.push_str(&rest[at..tail_start + tail_len]);
                }
                rest = &rest[tail_start + tail_len..];
                continue 'outer;
            }
        }
        out.push_str(rest);
        break;
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn redact_value_recurses_string_leaves_only() {
        let mut redactor = SecretRedactor::new();
        redactor.add_value("registered-secret-value-42");
        let mut value = json!({
            "path": "src/config.rs",
            "content": "let key = \"registered-secret-value-42\";",
            "nested": ["plain", "sk-or-v1-abcdefghijklmnopqrstuvwx"],
            "count": 7
        });
        redactor.redact_value(&mut value);
        assert_eq!(value["path"], "src/config.rs");
        assert!(!value["content"]
            .as_str()
            .unwrap()
            .contains("registered-secret-value-42"));
        assert!(value["content"]
            .as_str()
            .unwrap()
            .contains("[redacted-secret]"));
        // token shape caught inside a nested array
        assert!(!value["nested"][1]
            .as_str()
            .unwrap()
            .contains("sk-or-v1-abcd"));
        // non-string leaves and keys untouched
        assert_eq!(value["count"], 7);
        assert!(value.as_object().unwrap().contains_key("content"));
    }

    #[test]
    fn known_values_are_replaced_everywhere() {
        let mut redactor = SecretRedactor::new();
        redactor.add_value("sup3r-secret-value-123");
        let out = redactor.redact("a sup3r-secret-value-123 b sup3r-secret-value-123");
        assert_eq!(out, format!("a {REDACTED} b {REDACTED}"));
    }

    #[test]
    fn short_values_are_not_registered() {
        let mut redactor = SecretRedactor::new();
        redactor.add_value("short");
        assert_eq!(redactor.redact("a short b"), "a short b");
    }

    #[test]
    fn foreign_openrouter_key_is_caught_by_shape() {
        // The live incident: a foreign secrets file read via a granted shell
        // command — euler never knew the value, the shape must catch it.
        let redactor = SecretRedactor::new();
        let text = r#"{"name": "OPENROUTER_API_KEY", "value": "sk-or-v1-597ab1cbbc96dfffffffffffffffffff"}"#;
        let out = redactor.redact(text);
        assert!(!out.contains("sk-or-v1-597a"), "{out}");
        assert!(out.contains(REDACTED));
    }

    #[test]
    fn common_token_shapes_are_caught() {
        let redactor = SecretRedactor::new();
        // Fixtures are concatenated at runtime so no token-shaped literal
        // lives in the source tree (GitHub push protection flags them —
        // which is this feature working, one layer up).
        for token in [
            format!("sk-ant-{}", "api03-abcdefghijklmnop"),
            format!("ghp_{}", "0123456789abcdefghij"),
            format!("github_pat_{}", "11ABCDEFG0123456789abc"),
            format!("AKIA{}", "IOSFODNN7EXAMPLE"),
            format!("AIza{}", "SyA-1234567890abcdefghijklmnopqrstu"),
            format!("xoxb-{}", "123456789012-abcdefghijklmnop"),
            format!("xai-{}", "0123456789abcdefghijklmn"),
        ] {
            let token = token.as_str();
            let out = redactor.redact(&format!("before {token} after"));
            assert!(!out.contains(token), "not redacted: {token}");
            assert!(out.starts_with("before ") && out.ends_with(" after"));
        }
    }

    #[test]
    fn short_tails_and_prose_survive() {
        let redactor = SecretRedactor::new();
        // Not credentials: prefix present but tail too short, or split.
        for text in [
            "the ghp_ prefix alone",
            "sk-ant- is the anthropic prefix",
            "AKIA is how AWS keys start",
            "plain prose with no tokens at all",
        ] {
            assert_eq!(redactor.redact(text), text, "mangled: {text}");
        }
    }

    #[test]
    fn multiple_tokens_in_one_text() {
        let redactor = SecretRedactor::new();
        let text = format!(
            "a ghp_{} b sk-or-v1-{} c",
            "0123456789abcdefghij", "abcdefghijklmnop"
        );
        let out = redactor.redact(&text);
        assert_eq!(out, format!("a {REDACTED} b {REDACTED} c"));
    }
}
