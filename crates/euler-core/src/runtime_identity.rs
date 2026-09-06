use euler_event::{EventEnvelope, EventKind, JsonObject};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use thiserror::Error;

pub const RUNTIME_IDENTITY_SCHEMA_VERSION: u64 = 1;
const MAX_IDENTITY_STRING_BYTES: usize = 16 * 1024;
const MAX_ATTACHED_ROOTS: usize = 16;

/// Exact build and non-secret session-start projection identity recorded by a
/// new session. Git fields are explicit nulls when the build environment did
/// not contain a trustworthy checkout; that is distinct from a legacy stream
/// which has no runtime record at all.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeIdentity {
    pub schema_version: u64,
    pub binary_name: String,
    pub package_version: String,
    pub git_sha: Option<String>,
    pub git_dirty: Option<bool>,
    pub build_features: Vec<String>,
    pub session_start_projection_sha256: String,
    pub provider_client_version: String,
    pub attached_roots: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", content = "identity", rename_all = "snake_case")]
pub enum RecordedRuntimeIdentity {
    LegacyUnknown,
    Recorded(RuntimeIdentity),
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RuntimeIdentityError {
    #[error("session runtime identity is malformed: {0}")]
    Malformed(String),
    #[error("session runtime identity is invalid: {0}")]
    Invalid(&'static str),
}

impl RuntimeIdentity {
    pub(crate) fn current(
        session_start_projection_sha256: String,
        attached_roots: Vec<String>,
    ) -> Self {
        let raw_sha = env!("EULER_BUILD_GIT_SHA");
        let raw_dirty = env!("EULER_BUILD_GIT_DIRTY");
        let (git_sha, git_dirty) = match (raw_sha, raw_dirty) {
            ("unknown", "unknown") => (None, None),
            (sha, "true") => (Some(sha.to_owned()), Some(true)),
            (sha, "false") => (Some(sha.to_owned()), Some(false)),
            _ => (None, None),
        };
        let build_features = env!("EULER_BUILD_FEATURES")
            .split(',')
            .filter(|feature| !feature.is_empty())
            .map(str::to_owned)
            .collect();
        Self {
            schema_version: RUNTIME_IDENTITY_SCHEMA_VERSION,
            binary_name: "euler".to_owned(),
            package_version: env!("CARGO_PKG_VERSION").to_owned(),
            git_sha,
            git_dirty,
            build_features,
            session_start_projection_sha256,
            provider_client_version: euler_provider::CLIENT_VERSION.to_owned(),
            attached_roots,
        }
    }

    fn validate(&self) -> Result<(), RuntimeIdentityError> {
        if self.schema_version != RUNTIME_IDENTITY_SCHEMA_VERSION {
            return Err(RuntimeIdentityError::Invalid(
                "unsupported runtime identity schema version",
            ));
        }
        validate_nonempty(&self.binary_name, "binary name is empty or too large")?;
        validate_nonempty(
            &self.package_version,
            "package version is empty or too large",
        )?;
        validate_nonempty(
            &self.provider_client_version,
            "provider client version is empty or too large",
        )?;
        if !is_sha256(&self.session_start_projection_sha256) {
            return Err(RuntimeIdentityError::Invalid(
                "session.start projection identity is not a lowercase SHA-256 digest",
            ));
        }
        match (&self.git_sha, self.git_dirty) {
            (None, None) => {}
            (Some(sha), Some(_)) if is_git_oid(sha) => {}
            (Some(_), Some(_)) => {
                return Err(RuntimeIdentityError::Invalid(
                    "Git revision is not a lowercase object id",
                ))
            }
            _ => {
                return Err(RuntimeIdentityError::Invalid(
                    "Git revision and dirty state must be known or unknown together",
                ))
            }
        }
        if self.attached_roots.is_empty() || self.attached_roots.len() > MAX_ATTACHED_ROOTS {
            return Err(RuntimeIdentityError::Invalid(
                "attached roots are empty or exceed the supported bound",
            ));
        }
        let mut roots = BTreeSet::new();
        for root in &self.attached_roots {
            validate_nonempty(root, "an attached root is empty or too large")?;
            if !roots.insert(root) {
                return Err(RuntimeIdentityError::Invalid(
                    "attached roots contain a duplicate",
                ));
            }
        }
        if !self.build_features.windows(2).all(|pair| pair[0] < pair[1])
            || self.build_features.iter().any(|feature| {
                feature.is_empty()
                    || feature.len() > 128
                    || !feature.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
            })
        {
            return Err(RuntimeIdentityError::Invalid(
                "build features are not sorted, unique canonical names",
            ));
        }
        Ok(())
    }
}

/// Project the originating runtime for a provenance report or resume. A
/// legacy omission is reported explicitly and a present malformed record is
/// never replaced with the identity of the reader.
pub fn runtime_identity_from_events(
    events: &[EventEnvelope],
) -> Result<RecordedRuntimeIdentity, RuntimeIdentityError> {
    let mut starts = events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::SESSION_START);
    let Some(start) = starts.next() else {
        return Ok(RecordedRuntimeIdentity::LegacyUnknown);
    };
    if starts.next().is_some() {
        return Err(RuntimeIdentityError::Invalid(
            "session contains multiple session.start events",
        ));
    }
    let Some(value) = start.payload.get("runtime") else {
        return Ok(RecordedRuntimeIdentity::LegacyUnknown);
    };
    let identity = serde_json::from_value::<RuntimeIdentity>(value.clone())
        .map_err(|error| RuntimeIdentityError::Malformed(error.to_string()))?;
    identity.validate()?;
    let mut configuration = start.payload.clone();
    configuration.remove("runtime");
    let config_bytes = serde_json::to_vec(&configuration)
        .map_err(|error| RuntimeIdentityError::Malformed(error.to_string()))?;
    let expected_projection_sha = format!("{:x}", Sha256::digest(config_bytes));
    if identity.session_start_projection_sha256 != expected_projection_sha {
        return Err(RuntimeIdentityError::Invalid(
            "session.start projection digest does not match its payload",
        ));
    }
    let root = start
        .payload
        .get("root")
        .and_then(serde_json::Value::as_str)
        .ok_or(RuntimeIdentityError::Invalid(
            "current runtime identity requires a recorded root",
        ))?;
    if identity.attached_roots.as_slice() != [root] {
        return Err(RuntimeIdentityError::Invalid(
            "attached roots do not match the session root",
        ));
    }
    Ok(RecordedRuntimeIdentity::Recorded(identity))
}

/// Recompute a `session.start` event's stored projection digest in place
/// after its payload was legitimately rewritten (e.g. by scrub redacting a
/// value that happened to appear in a non-runtime field such as the recorded
/// root). Scrub is a writer-owned, audited mutation, not external tampering,
/// so keeping the digest in sync here preserves the check's purpose —
/// detecting payload drift from *outside* the writer — without leaving every
/// rescrubbed session permanently `Invalid`. A payload with no `runtime`
/// object, or one that no longer deserializes as `RuntimeIdentity`, is left
/// untouched; `runtime_identity_from_events` reports that state on its own
/// terms.
pub(crate) fn resync_session_start_projection_digest(payload: &mut JsonObject) {
    let Some(runtime_value) = payload.get("runtime").cloned() else {
        return;
    };
    let Ok(mut identity) = serde_json::from_value::<RuntimeIdentity>(runtime_value) else {
        return;
    };
    let mut configuration = payload.clone();
    configuration.remove("runtime");
    let Ok(config_bytes) = serde_json::to_vec(&configuration) else {
        return;
    };
    identity.session_start_projection_sha256 = format!("{:x}", Sha256::digest(config_bytes));
    if let Ok(runtime_value) = serde_json::to_value(identity) {
        payload.insert("runtime".to_owned(), runtime_value);
    }
}

fn validate_nonempty(value: &str, message: &'static str) -> Result<(), RuntimeIdentityError> {
    if value.is_empty()
        || value.len() > MAX_IDENTITY_STRING_BYTES
        || value.chars().any(char::is_control)
    {
        Err(RuntimeIdentityError::Invalid(message))
    } else {
        Ok(())
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_git_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use euler_event::object;
    use serde_json::json;

    #[test]
    fn missing_runtime_record_is_explicitly_legacy_unknown() {
        let start = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::SESSION_START,
            object([("provider", "fixture".into()), ("model", "echo".into())]),
        );
        assert_eq!(
            runtime_identity_from_events(&[start]).expect("legacy identity"),
            RecordedRuntimeIdentity::LegacyUnknown
        );
        assert_eq!(
            serde_json::to_value(RecordedRuntimeIdentity::LegacyUnknown)
                .expect("legacy report identity"),
            json!({"status": "legacy_unknown"})
        );
    }

    #[test]
    fn malformed_current_identity_is_not_treated_as_legacy() {
        let start = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::SESSION_START,
            object([
                ("provider", "fixture".into()),
                ("model", "echo".into()),
                ("runtime", json!({"schema_version": 1})),
            ]),
        );
        assert!(matches!(
            runtime_identity_from_events(&[start]),
            Err(RuntimeIdentityError::Malformed(_))
        ));
    }

    #[test]
    fn duplicate_session_start_is_invalid_instead_of_selecting_one_identity() {
        let first = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::SESSION_START,
            object([("provider", "fixture".into()), ("model", "echo".into())]),
        );
        let second = EventEnvelope::new(
            "session",
            "root",
            Some(first.id.clone()),
            EventKind::SESSION_START,
            object([("provider", "other".into()), ("model", "other".into())]),
        );
        assert_eq!(
            runtime_identity_from_events(&[first, second]),
            Err(RuntimeIdentityError::Invalid(
                "session contains multiple session.start events"
            ))
        );
    }

    #[test]
    fn runtime_identity_binds_configuration_and_single_attached_root() {
        let mut payload = object([
            ("provider", "fixture".into()),
            ("model", "echo".into()),
            ("root", "/workspace".into()),
        ]);
        let projection_digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&payload).expect("configuration"))
        );
        let runtime = RuntimeIdentity::current(projection_digest, vec!["/workspace".to_owned()]);
        payload.insert(
            "runtime".to_owned(),
            serde_json::to_value(runtime).expect("runtime"),
        );
        let start = EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::SESSION_START,
            payload.clone(),
        );
        let report = runtime_identity_from_events(&[start]).expect("recorded identity");
        assert!(matches!(&report, RecordedRuntimeIdentity::Recorded(_)));
        let report_json = serde_json::to_value(report).expect("recorded report identity");
        assert_eq!(report_json["status"], "recorded");
        assert_eq!(
            report_json["identity"]["attached_roots"],
            json!(["/workspace"])
        );

        payload.insert("model".to_owned(), "changed".into());
        let changed =
            EventEnvelope::new("session", "root", None, EventKind::SESSION_START, payload);
        assert_eq!(
            runtime_identity_from_events(&[changed]),
            Err(RuntimeIdentityError::Invalid(
                "session.start projection digest does not match its payload"
            ))
        );
    }
}
