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
/// Non-secret label for a match against a registered known value. Safe to
/// record in provenance; the value itself never is.
const KNOWN_VALUE_LABEL: &str = "known-value";
/// Replacement written by the scrub operation (issue #100), kept distinct from
/// the emit-time `[redacted-secret]` marker so a reader can tell WHICH
/// mechanism removed a value: automatic entry-boundary redaction, or a
/// deliberate user scrub after the fact.
pub const SCRUBBED: &str = "[scrubbed]";

/// Replace every occurrence of each value in `secrets` with the scrub marker,
/// returning the rewritten text and the number of replacements made. A free
/// function on purpose: scrub acts on explicit values a caller already holds,
/// independent of any registered redactor state.
pub fn scrub_secrets_in_text(text: &str, secrets: &[String]) -> (String, usize) {
    let mut out = text.to_owned();
    let mut replacements = 0;
    for secret in secrets {
        if secret.is_empty() || !out.contains(secret.as_str()) {
            continue;
        }
        replacements += out.matches(secret.as_str()).count();
        out = out.replace(secret.as_str(), SCRUBBED);
    }
    (out, replacements)
}

/// Scrub literal and JSON-escaped forms from arbitrary bytes. Extension
/// artifacts can be binary or can embed JSON inside HTML/JavaScript, where a
/// value containing `"` or `\\` no longer appears as its literal UTF-8 bytes.
pub fn scrub_secrets_in_bytes(bytes: &[u8], secrets: &[String]) -> (Vec<u8>, usize) {
    let mut out = bytes.to_vec();
    let mut replacements = 0;
    for secret in secrets {
        if secret.is_empty() {
            continue;
        }
        let (next, count) = replace_bytes(&out, secret.as_bytes(), SCRUBBED.as_bytes());
        out = next;
        replacements += count;

        let encoded = serde_json::to_string(secret).unwrap_or_default();
        let encoded = encoded
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or_default();
        if !encoded.is_empty() && encoded.as_bytes() != secret.as_bytes() {
            let (next, count) = replace_bytes(&out, encoded.as_bytes(), SCRUBBED.as_bytes());
            out = next;
            replacements += count;
        }
    }
    (out, replacements)
}

pub(crate) fn replace_bytes(input: &[u8], needle: &[u8], replacement: &[u8]) -> (Vec<u8>, usize) {
    if needle.is_empty() || needle.len() > input.len() {
        return (input.to_vec(), 0);
    }
    let mut out = Vec::with_capacity(input.len());
    let mut from = 0;
    let mut replacements = 0;
    while let Some(offset) = input[from..]
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let at = from + offset;
        out.extend_from_slice(&input[from..at]);
        out.extend_from_slice(replacement);
        from = at + needle.len();
        replacements += 1;
    }
    if replacements == 0 {
        return (input.to_vec(), 0);
    }
    out.extend_from_slice(&input[from..]);
    (out, replacements)
}

/// Recursively scrub secrets from every string leaf and object key of a JSON
/// value, returning the replacement count. The inline `projection_blob`
/// compaction state is a string leaf and so is covered. This is the single
/// scrub walk shared by the ledger rewrite and the in-memory bus.
pub fn scrub_secrets_in_value(value: &mut serde_json::Value, secrets: &[String]) -> usize {
    let mut count = 0;
    scrub_value_rec(value, secrets, &mut count);
    count
}

/// Scrub a JSON object including its top-level keys. Event payloads use the
/// object alias directly rather than a wrapping `Value`.
pub fn scrub_secrets_in_object(object: &mut euler_event::JsonObject, secrets: &[String]) -> usize {
    let mut value = serde_json::Value::Object(std::mem::take(object));
    let count = scrub_secrets_in_value(&mut value, secrets);
    let serde_json::Value::Object(scrubbed) = value else {
        unreachable!("an object scrub preserves the JSON value kind")
    };
    *object = scrubbed;
    count
}

fn scrub_value_rec(value: &mut serde_json::Value, secrets: &[String], count: &mut usize) {
    match value {
        serde_json::Value::String(text) => {
            let (scrubbed, replacements) = scrub_secrets_in_text(text, secrets);
            if replacements > 0 {
                *text = scrubbed;
                *count += replacements;
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                scrub_value_rec(item, secrets, count);
            }
        }
        serde_json::Value::Object(map) => {
            let entries = std::mem::take(map);
            for (key, mut value) in entries {
                let (scrubbed_key, replacements) = scrub_secrets_in_text(&key, secrets);
                *count += replacements;
                scrub_value_rec(&mut value, secrets, count);
                let key = unique_json_key(map, scrubbed_key);
                map.insert(key, value);
            }
        }
        _ => {}
    }
}

pub(crate) fn unique_json_key(
    map: &serde_json::Map<String, serde_json::Value>,
    key: String,
) -> String {
    if !map.contains_key(&key) {
        return key;
    }
    for suffix in 2_u64.. {
        let candidate = format!("{key}#{suffix}");
        if !map.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!("u64 key suffix space exhausted")
}

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

/// Clones SHARE one value set: the session hands clones to companion loops,
/// the extension host, and provider secret sinks, and a value registered at
/// runtime through any handle (e.g. a custom provider resolving an
/// `x-secret` at request time, possibly on a reviewer worker thread) must be
/// visible to every emission site immediately.
#[derive(Clone, Debug, Default)]
pub struct SecretRedactor {
    values: std::sync::Arc<std::sync::RwLock<Vec<String>>>,
}

impl SecretRedactor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed from the configured secret environment variables.
    pub fn from_env() -> Self {
        let redactor = Self::new();
        for name in SECRET_ENV_NAMES {
            if let Ok(value) = std::env::var(name) {
                redactor.add_value(value);
            }
        }
        redactor
    }

    /// Add a known secret value (auth-file credential, resolved `x-secret`).
    /// Short values are ignored — see `MIN_VALUE_LEN`. Shared: every clone
    /// of this redactor observes the addition.
    pub fn add_value(&self, value: impl Into<String>) {
        let value = value.into();
        if value.len() < MIN_VALUE_LEN {
            return;
        }
        let mut values = self.values.write().unwrap_or_else(|poison| {
            // A panic mid-push cannot corrupt Vec<String> contents; losing
            // redaction values to poisoning would be the unsafe direction.
            poison.into_inner()
        });
        if !values.contains(&value) {
            values.push(value);
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

    /// Redact known values and known token shapes from `text`.
    pub fn redact(&self, text: &str) -> String {
        let values = self
            .values
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out = values
            .iter()
            .fold(text.to_owned(), |acc, value| acc.replace(value, REDACTED));
        out = redact_token_shapes(&out);
        out
    }

    /// READ-ONLY detection: report registered known values and credential
    /// shapes present in `text`, without modifying anything. Each match's
    /// `label` is a non-secret descriptor safe to record in provenance; the
    /// `value` is the matched text and must stay in memory only. Used by the
    /// exposure warning (labels) and the scrub operation (values).
    pub fn detect(&self, text: &str) -> Vec<SecretMatch> {
        let mut matches: Vec<SecretMatch> = Vec::new();
        {
            let values = self
                .values
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for value in values.iter() {
                if text.contains(value.as_str()) {
                    matches.push(SecretMatch {
                        label: KNOWN_VALUE_LABEL.to_owned(),
                        value: value.clone(),
                    });
                }
            }
        }
        for (span, prefix) in token_shape_spans(text) {
            matches.push(SecretMatch {
                label: prefix.to_owned(),
                value: text[span].to_owned(),
            });
        }
        // Same secret can match multiple ways (known value + shape); collapse
        // so callers scrub and count each distinct value once.
        matches.sort_by(|a, b| a.value.cmp(&b.value).then_with(|| a.label.cmp(&b.label)));
        matches.dedup_by(|a, b| a.value == b.value);
        matches
    }

    /// Detect credentials in every JSON string leaf and object key without
    /// serializing first. Serializing escapes quotes and backslashes, which can
    /// hide an exact registered value from the known-value detector.
    pub fn detect_value(&self, value: &serde_json::Value) -> Vec<SecretMatch> {
        let mut matches = Vec::new();
        self.detect_value_rec(value, &mut matches);
        matches.sort_by(|a, b| a.value.cmp(&b.value).then_with(|| a.label.cmp(&b.label)));
        matches.dedup_by(|a, b| a.value == b.value);
        matches
    }

    fn detect_value_rec(&self, value: &serde_json::Value, matches: &mut Vec<SecretMatch>) {
        match value {
            serde_json::Value::String(text) => matches.extend(self.detect(text)),
            serde_json::Value::Array(items) => {
                for item in items {
                    self.detect_value_rec(item, matches);
                }
            }
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    matches.extend(self.detect(key));
                    self.detect_value_rec(value, matches);
                }
            }
            _ => {}
        }
    }
}

/// A credential detected in a payload: a non-secret `label` (shape prefix or
/// `known-value`) plus the matched `value` (in-memory only, never persisted).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretMatch {
    pub label: String,
    pub value: String,
}

fn token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'
}

/// Byte spans of `prefix + [A-Za-z0-9_-]{MIN_TOKEN_TAIL,}` runs, earliest-first
/// and non-overlapping, each paired with the prefix that matched. The single
/// source of truth for both redaction and detection.
fn token_shape_spans(text: &str) -> Vec<(std::ops::Range<usize>, &'static str)> {
    let mut spans = Vec::new();
    let mut from = 0;
    while from < text.len() {
        let Some((at, hit)) = TOKEN_PREFIXES
            .iter()
            .filter_map(|prefix| text[from..].find(prefix).map(|at| (from + at, *prefix)))
            .min_by_key(|(at, _)| *at)
        else {
            break;
        };
        let tail_start = at + hit.len();
        let tail_len = text[tail_start..]
            .chars()
            .take_while(|ch| token_char(*ch))
            .map(char::len_utf8)
            .sum::<usize>();
        if tail_len >= MIN_TOKEN_TAIL {
            spans.push((at..tail_start + tail_len, hit));
        }
        // `hit.len()` >= 4, so this always advances past the current prefix.
        from = tail_start + tail_len;
    }
    spans
}

/// Replace `prefix + [A-Za-z0-9_-]{MIN_TOKEN_TAIL,}` runs with the marker.
fn redact_token_shapes(text: &str) -> String {
    let spans = token_shape_spans(text);
    if spans.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (span, _) in spans {
        out.push_str(&text[last..span.start]);
        out.push_str(REDACTED);
        last = span.end;
    }
    out.push_str(&text[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values_are_replaced_everywhere() {
        let redactor = SecretRedactor::new();
        redactor.add_value("sup3r-secret-value-123");
        let out = redactor.redact("a sup3r-secret-value-123 b sup3r-secret-value-123");
        assert_eq!(out, format!("a {REDACTED} b {REDACTED}"));
    }

    #[test]
    fn clones_share_the_value_set() {
        // The session hands redactor clones to companion loops, the
        // extension host, and provider secret sinks; a value registered at
        // request time through any handle must reach every emission site.
        let redactor = SecretRedactor::new();
        let handle = redactor.clone();
        handle.add_value("runtime-resolved-secret-1");
        let out = redactor.redact("a runtime-resolved-secret-1 b");
        assert_eq!(out, format!("a {REDACTED} b"));
    }

    #[test]
    fn short_values_are_not_registered() {
        let redactor = SecretRedactor::new();
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

    #[test]
    fn detect_reports_shapes_and_values_without_mutating() {
        let redactor = SecretRedactor::new();
        redactor.add_value("known-secret-value-1");
        let token = format!("ghp_{}", "0123456789abcdefghij");
        let text = format!("curl -H auth:{token} for known-secret-value-1");
        let hits = redactor.detect(&text);
        // Two distinct secrets: the known value and the token shape.
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert!(hits
            .iter()
            .any(|h| h.label == "known-value" && h.value == "known-secret-value-1"));
        assert!(hits.iter().any(|h| h.label == "ghp_" && h.value == token));
    }

    #[test]
    fn detect_labels_never_carry_the_value() {
        let redactor = SecretRedactor::new();
        let token = format!("sk-ant-{}", "api03-abcdefghijklmnop");
        let hits = redactor.detect(&format!("leaked {token}"));
        assert_eq!(hits.len(), 1);
        // The label is the shape prefix, never the secret text.
        assert_eq!(hits[0].label, "sk-ant-");
        assert!(!hits[0].label.contains("api03"));
    }

    #[test]
    fn detect_collapses_the_same_value_matched_two_ways() {
        let redactor = SecretRedactor::new();
        let token = format!("xai-{}", "0123456789abcdefghijklmn");
        // Register the exact token as a known value too — it matches both ways.
        redactor.add_value(token.clone());
        let hits = redactor.detect(&format!("here {token} there"));
        assert_eq!(hits.len(), 1, "one distinct value, not two: {hits:?}");
        assert_eq!(hits[0].value, token);
    }

    #[test]
    fn detect_is_quiet_on_clean_text() {
        let redactor = SecretRedactor::new();
        assert!(redactor
            .detect("plain prose, ghp_ alone, AKIA short")
            .is_empty());
    }

    #[test]
    fn detect_value_finds_registered_secrets_with_json_metacharacters() {
        let redactor = SecretRedactor::new();
        let secret = "registered-\"secret\\value-123";
        redactor.add_value(secret);
        let input = serde_json::json!({"nested": [secret]});

        let hits = redactor.detect_value(&input);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].value, secret);
    }

    #[test]
    fn structured_scrub_covers_object_keys_without_dropping_collisions() {
        let secret = "credential-value-1234".to_owned();
        let mut map = serde_json::Map::new();
        map.insert(secret.clone(), "first".into());
        map.insert(SCRUBBED.to_owned(), "second".into());
        let mut value = serde_json::Value::Object(map);

        let replacements = scrub_secrets_in_value(&mut value, &[secret]);

        assert_eq!(replacements, 1);
        assert_eq!(value.as_object().expect("object").len(), 2);
        assert!(serde_json::to_string(&value)
            .expect("serialize")
            .contains(SCRUBBED));
    }

    #[test]
    fn object_scrub_covers_top_level_keys() {
        let secret = "credential-value-1234".to_owned();
        let mut object = euler_event::JsonObject::new();
        object.insert(secret.clone(), "value".into());

        let replacements = scrub_secrets_in_object(&mut object, &[secret]);

        assert_eq!(replacements, 1);
        assert_eq!(object.get(SCRUBBED), Some(&serde_json::json!("value")));
    }

    #[test]
    fn byte_scrub_covers_json_escaped_values() {
        let secret = "credential-\"value\\1234".to_owned();
        let encoded = serde_json::to_vec(&serde_json::json!({"value": secret})).expect("json");

        let (scrubbed, replacements) =
            scrub_secrets_in_bytes(&encoded, std::slice::from_ref(&secret));

        assert_eq!(replacements, 1);
        assert!(!String::from_utf8_lossy(&scrubbed).contains(&secret));
        serde_json::from_slice::<serde_json::Value>(&scrubbed).expect("valid JSON");
    }
}
