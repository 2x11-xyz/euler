//! Resume workspace relocation (ADR 0017 phase 3, project-context contract
//! "Resume relocation and consent"; `project.context.relocated` in
//! `docs/contracts/events.md`).
//!
//! This module owns the read and validation side of relocation: folding the
//! governing workspace identity and projected root from an accepted event
//! prefix, and validating a `project.context.relocated` event as untrusted
//! input. A forged or stale relocation never supersedes; the whole chain is
//! re-validated on every fold.
//!
//! The write side (presenting the card, appending the event) lives with the
//! interactive surface and the `--accept-relocation` flow; the payload builder
//! here produces exactly the bytes those callers append.

use super::digest::{
    workspace_identity_digest_v1, WORKSPACE_IDENTITY_ALGORITHM, WORKSPACE_IDENTITY_VERSION,
};
use euler_event::{EventEnvelope, EventKind, JsonObject};
use serde_json::Value;
use std::path::Path;

/// A validated workspace identity: `{ algorithm, version, digest }`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RelocationIdentity {
    pub algorithm: String,
    pub version: u64,
    pub digest: String,
}

impl RelocationIdentity {
    fn from_value(value: Option<&Value>) -> Option<Self> {
        let object = value?.as_object()?;
        if object.len() != 3 {
            return None;
        }
        let algorithm = object.get("algorithm")?.as_str()?.to_owned();
        let version = object.get("version")?.as_u64()?;
        let digest = object.get("digest")?.as_str()?.to_owned();
        if !is_hex_digest(&digest) {
            return None;
        }
        Some(Self {
            algorithm,
            version,
            digest,
        })
    }

    /// The `{ algorithm, version, digest }` JSON. Used by the write side (the
    /// acceptance path builds a payload) and by tests.
    fn to_value(&self) -> Value {
        serde_json::json!({
            "algorithm": self.algorithm,
            "version": self.version,
            "digest": self.digest,
        })
    }

    /// The identity of a canonical workspace root under the current algorithm.
    fn of_canonical_root(canonical_root: &Path) -> Self {
        Self {
            algorithm: WORKSPACE_IDENTITY_ALGORITHM.to_owned(),
            version: u64::from(WORKSPACE_IDENTITY_VERSION),
            digest: workspace_identity_digest_v1(canonical_root),
        }
    }
}

fn is_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

const RELOCATED_KEYS: &[&str] = &[
    "schema_version",
    "prior_identity",
    "new_identity",
    "new_root",
    "decided_at",
];

/// The relocation-record schema version.
pub(crate) const RELOCATED_SCHEMA_VERSION: u64 = 1;

/// A fully validated relocation event payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Relocation {
    pub prior_identity: RelocationIdentity,
    pub new_identity: RelocationIdentity,
    pub new_root: String,
}

/// Validate one `project.context.relocated` payload's shape, in isolation
/// (untrusted input). Chain and supersession rules are enforced separately in
/// [`fold_governing_identity`].
fn validate_relocated_shape(payload: &JsonObject) -> Result<Relocation, String> {
    for key in payload.keys() {
        if !RELOCATED_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "the relocation record carries a field this Euler version does not record: {key}"
            ));
        }
    }
    if payload.get("schema_version").and_then(Value::as_u64) != Some(RELOCATED_SCHEMA_VERSION) {
        return Err("the relocation record was written by a different Euler version".to_owned());
    }
    let prior_identity = RelocationIdentity::from_value(payload.get("prior_identity"))
        .ok_or_else(|| "the relocation record's prior identity is malformed".to_owned())?;
    let new_identity = RelocationIdentity::from_value(payload.get("new_identity"))
        .ok_or_else(|| "the relocation record's new identity is malformed".to_owned())?;
    let new_root = payload
        .get("new_root")
        .and_then(Value::as_str)
        .filter(|root| !root.is_empty())
        .ok_or_else(|| "the relocation record has no new folder path".to_owned())?
        .to_owned();
    if payload.get("decided_at").and_then(Value::as_str).is_none() {
        return Err("the relocation record has no acceptance stamp".to_owned());
    }
    // Both identities must use an algorithm this Euler version knows.
    for identity in [&prior_identity, &new_identity] {
        if identity.algorithm != WORKSPACE_IDENTITY_ALGORITHM
            || identity.version != u64::from(WORKSPACE_IDENTITY_VERSION)
        {
            return Err(
                "the relocation record uses a workspace identity algorithm this Euler version \
                 does not know"
                    .to_owned(),
            );
        }
    }
    // `new_root` MUST re-derive to `new_identity` under the identity algorithm.
    // The recorded root is already the canonical lossy display string, so
    // hashing its bytes reproduces the identity for the supported hosts.
    if workspace_identity_digest_v1(Path::new(&new_root)) != new_identity.digest {
        return Err(
            "the relocation record's new folder does not match its recorded identity".to_owned(),
        );
    }
    Ok(Relocation {
        prior_identity,
        new_identity,
        new_root,
    })
}

/// The identity that governs resume comparison after folding the accepted
/// event prefix: the latest valid `project.context.relocated` `new_identity`
/// if the relocation chain is valid, otherwise the base snapshot identity.
///
/// Returns `Ok(None)` for a legacy session with no snapshot identity. Any
/// malformed or stale relocation in the chain fails closed with a
/// plain-language reason (resume rejects rather than falling back).
pub(crate) fn fold_governing_identity(
    events: &[EventEnvelope],
    snapshot_identity: Option<RelocationIdentity>,
) -> Result<Option<RelocationIdentity>, String> {
    let mut governing = snapshot_identity;
    for event in events
        .iter()
        .filter(|event| event.kind.as_str() == EventKind::PROJECT_CONTEXT_RELOCATED)
    {
        let relocation = validate_relocated_shape(&event.payload)?;
        // The prior identity MUST equal the identity governing immediately
        // before this event. A stale or branched acceptance (its prior does
        // not match the current governing identity) is rejected and never
        // supersedes.
        match &governing {
            Some(current) if *current == relocation.prior_identity => {}
            Some(_) => {
                return Err(
                    "a relocation record does not line up with where the session actually ran; \
                     start a new session in this folder"
                        .to_owned(),
                )
            }
            None => {
                return Err(
                    "a relocation record references a workspace this session never recorded; \
                     start a new session in this folder"
                        .to_owned(),
                )
            }
        }
        governing = Some(relocation.new_identity);
    }
    Ok(governing)
}

/// The projected workspace root after folding: the latest valid relocation's
/// `new_root`, or `None` when there is no relocation. The caller uses this to
/// project the session's root everywhere the first `session.start` root is
/// used. Chain validity is the caller's concern (via
/// [`fold_governing_identity`]); this only extracts the latest recorded root.
pub(crate) fn projected_new_root(events: &[EventEnvelope]) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::PROJECT_CONTEXT_RELOCATED)
        .and_then(|event| event.payload.get("new_root"))
        .and_then(Value::as_str)
        .filter(|root| !root.is_empty())
        .map(str::to_owned)
}

/// The workspace identity governing the accepted event prefix (the snapshot
/// identity folded through any prior relocations), as the value a new
/// relocation records for its `prior_identity`. `None` for a legacy prefix
/// with no snapshot.
pub(crate) fn governing_identity_value(events: &[EventEnvelope]) -> Result<Option<Value>, String> {
    let base = snapshot_identity(events)?;
    Ok(fold_governing_identity(events, base)?.map(|identity| identity.to_value()))
}

/// Build the canonical `project.context.relocated` payload the acceptance path
/// appends. `new_root` is the same bounded lossy display form `session.start`
/// records for its root.
pub(crate) fn build_relocated_payload(
    prior_identity_value: &Value,
    live_canonical_root: &Path,
    new_root_display: String,
    decided_at: String,
) -> JsonObject {
    let new_identity = RelocationIdentity::of_canonical_root(live_canonical_root);
    euler_event::object([
        ("schema_version", RELOCATED_SCHEMA_VERSION.into()),
        ("prior_identity", prior_identity_value.clone()),
        ("new_identity", new_identity.to_value()),
        ("new_root", new_root_display.into()),
        ("decided_at", decided_at.into()),
    ])
}

/// Extract the workspace identity from the latest snapshot event, as the base
/// for [`fold_governing_identity`]. Returns `None` for a legacy session and an
/// error for a snapshot whose identity is malformed or uses an unknown
/// algorithm.
pub(crate) fn snapshot_identity(
    events: &[EventEnvelope],
) -> Result<Option<RelocationIdentity>, String> {
    let Some(snapshot) = events
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::PROJECT_CONTEXT_SNAPSHOT)
    else {
        return Ok(None);
    };
    let identity = RelocationIdentity::from_value(snapshot.payload.get("workspace_identity"))
        .ok_or_else(|| "the session's workspace record cannot be read".to_owned())?;
    if identity.algorithm != WORKSPACE_IDENTITY_ALGORITHM
        || identity.version != u64::from(WORKSPACE_IDENTITY_VERSION)
    {
        return Err("the session's workspace record uses an unknown algorithm".to_owned());
    }
    Ok(Some(identity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_root::session_root_for_event;

    fn identity_of(path: &Path) -> RelocationIdentity {
        RelocationIdentity::of_canonical_root(path)
    }

    fn relocated_event(parent: &str, prior: &RelocationIdentity, new_root: &Path) -> EventEnvelope {
        let payload = build_relocated_payload(
            &prior.to_value(),
            new_root,
            session_root_for_event(new_root),
            "2026-07-21T00:00:00Z".to_owned(),
        );
        EventEnvelope::new(
            "session",
            "root",
            Some(parent.to_owned()),
            EventKind::PROJECT_CONTEXT_RELOCATED,
            payload,
        )
    }

    fn snapshot_event(identity: &RelocationIdentity) -> EventEnvelope {
        EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::PROJECT_CONTEXT_SNAPSHOT,
            euler_event::object([("workspace_identity", identity.to_value())]),
        )
    }

    #[test]
    fn valid_relocation_supersedes_identity_and_projects_root() {
        let temp = tempfile::tempdir().expect("temp");
        let old = temp.path().join("old");
        let new = temp.path().join("new");
        std::fs::create_dir_all(&old).expect("old");
        std::fs::create_dir_all(&new).expect("new");
        let old = std::fs::canonicalize(&old).expect("c-old");
        let new = std::fs::canonicalize(&new).expect("c-new");
        let old_identity = identity_of(&old);
        let events = vec![
            snapshot_event(&old_identity),
            relocated_event("snap", &old_identity, &new),
        ];
        let base = snapshot_identity(&events).expect("ok").expect("some");
        assert_eq!(base, old_identity);
        let governing = fold_governing_identity(&events, Some(base))
            .expect("valid chain")
            .expect("identity");
        assert_eq!(governing, identity_of(&new), "new identity governs");
        assert_eq!(
            projected_new_root(&events).as_deref(),
            Some(session_root_for_event(&new).as_str())
        );
    }

    #[test]
    fn stale_prior_identity_rejects_and_never_supersedes() {
        let temp = tempfile::tempdir().expect("temp");
        let old = std::fs::canonicalize(temp.path()).expect("c");
        let new = old.join("elsewhere");
        // Forge a relocation whose prior identity is not the snapshot identity.
        let wrong_prior = RelocationIdentity {
            algorithm: WORKSPACE_IDENTITY_ALGORITHM.to_owned(),
            version: u64::from(WORKSPACE_IDENTITY_VERSION),
            digest: "c".repeat(64),
        };
        let events = vec![
            snapshot_event(&identity_of(&old)),
            relocated_event("snap", &wrong_prior, &new),
        ];
        let base = snapshot_identity(&events).expect("ok");
        let error = fold_governing_identity(&events, base).expect_err("stale rejects");
        assert!(error.contains("start a new session"));
    }

    #[test]
    fn forged_new_root_that_does_not_match_identity_rejects() {
        let temp = tempfile::tempdir().expect("temp");
        let old = std::fs::canonicalize(temp.path()).expect("c");
        let old_identity = identity_of(&old);
        // A payload whose new_identity does not match new_root.
        let mut payload = build_relocated_payload(
            &old_identity.to_value(),
            &old.join("real-new"),
            "/some/other/path".to_owned(),
            "2026-07-21T00:00:00Z".to_owned(),
        );
        // new_identity now describes `real-new`, but new_root says something
        // else: re-derivation must fail.
        payload.insert("new_root".to_owned(), "/some/other/path".into());
        let event = EventEnvelope::new(
            "session",
            "root",
            Some("snap".to_owned()),
            EventKind::PROJECT_CONTEXT_RELOCATED,
            payload,
        );
        let events = vec![snapshot_event(&old_identity), event];
        let base = snapshot_identity(&events).expect("ok");
        let error = fold_governing_identity(&events, base).expect_err("mismatch rejects");
        assert!(error.contains("does not match"));
    }

    #[test]
    fn unknown_field_on_relocation_rejects() {
        let temp = tempfile::tempdir().expect("temp");
        let old = std::fs::canonicalize(temp.path()).expect("c");
        let old_identity = identity_of(&old);
        let new = old.join("new");
        let mut payload = build_relocated_payload(
            &old_identity.to_value(),
            &new,
            session_root_for_event(&new),
            "2026-07-21T00:00:00Z".to_owned(),
        );
        payload.insert("smuggled".to_owned(), "payload".into());
        let event = EventEnvelope::new(
            "session",
            "root",
            Some("snap".to_owned()),
            EventKind::PROJECT_CONTEXT_RELOCATED,
            payload,
        );
        let events = vec![snapshot_event(&old_identity), event];
        let base = snapshot_identity(&events).expect("ok");
        assert!(fold_governing_identity(&events, base).is_err());
    }

    #[test]
    fn chained_relocations_take_the_latest() {
        let temp = tempfile::tempdir().expect("temp");
        let a = std::fs::canonicalize(temp.path()).expect("c");
        let b = a.join("b");
        let c = a.join("c");
        let id_a = identity_of(&a);
        let id_b = identity_of(&b);
        let events = vec![
            snapshot_event(&id_a),
            relocated_event("snap", &id_a, &b),
            relocated_event("reloc1", &id_b, &c),
        ];
        let base = snapshot_identity(&events).expect("ok").expect("some");
        let governing = fold_governing_identity(&events, Some(base))
            .expect("valid")
            .expect("identity");
        assert_eq!(governing, identity_of(&c));
        assert_eq!(
            projected_new_root(&events).as_deref(),
            Some(session_root_for_event(&c).as_str())
        );
    }

    #[test]
    fn no_relocation_keeps_the_snapshot_identity() {
        let temp = tempfile::tempdir().expect("temp");
        let root = std::fs::canonicalize(temp.path()).expect("c");
        let identity = identity_of(&root);
        let events = vec![snapshot_event(&identity)];
        let base = snapshot_identity(&events).expect("ok");
        assert_eq!(
            fold_governing_identity(&events, base).expect("ok"),
            Some(identity)
        );
        assert_eq!(projected_new_root(&events), None);
    }
}
