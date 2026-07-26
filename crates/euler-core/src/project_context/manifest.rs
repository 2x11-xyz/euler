//! Candidate manifest: the complete bounded preflight result and its
//! canonical, versioned JSON encoding.
//!
//! The canonical encoding is authoritative: the portable candidate digest
//! commits to these exact bytes, and rehydration re-validates them strictly
//! (duplicate keys, trailing data, unknown fields, unsupported versions,
//! limit violations, and internal digest mismatches all reject).

use super::digest::source_digest_v1;
use super::{
    MAX_COMBINED_EULER_MD_BYTES, MAX_COMBINED_SKILL_BODY_BYTES, MAX_EULER_MD_BYTES,
    MAX_EULER_MD_SOURCES, MAX_IDENTITY_BYTES, MAX_MANIFEST_DIAGNOSTICS, MAX_SKILLS,
    MAX_SKILL_BODY_BYTES, MAX_SKILL_DESCRIPTION_BYTES, MAX_SKILL_NAME_BYTES,
};
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub(crate) const LEGACY_MANIFEST_VERSION: u32 = 1;
pub(crate) const MANIFEST_VERSION: u32 = 2;

/// One accepted `EULER.md` source: normalized project-root-relative identity
/// plus the frozen post-redaction content and its domain-separated digest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestSource {
    pub path: String,
    pub byte_len: u64,
    pub digest: String,
    pub content: String,
}

/// The scope that owns a frozen skill package. Scope is data, not authority:
/// both variants are model-facing guidance and neither grants permissions.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SkillScope {
    User,
    Project,
}

impl SkillScope {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// One accepted skill package. `body` is the frozen, post-redaction Markdown
/// below the frontmatter; catalog metadata stays separate so the body remains
/// out of model input until `skill_read` selects it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestSkill {
    pub name: String,
    pub description: String,
    pub scope: SkillScope,
    pub path: String,
    pub body_len: u64,
    pub body_digest: String,
    pub body: String,
}

/// One ordered, content-free diagnostic record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestDiagnostic {
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<u64>,
}

/// The complete bounded preflight result. Field order is the canonical
/// encoding order; do not reorder fields.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CandidateManifest {
    pub version: u32,
    pub sources: Vec<ManifestSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<ManifestSkill>,
    pub diagnostics: Vec<ManifestDiagnostic>,
    pub reason_counts: BTreeMap<String, u64>,
}

impl CandidateManifest {
    pub(crate) fn project_acknowledgment_view(&self) -> Self {
        let mut manifest = Self {
            version: self.version,
            sources: self.sources.clone(),
            skills: self
                .skills
                .iter()
                .filter(|skill| skill.scope == SkillScope::Project)
                .cloned()
                .collect(),
            diagnostics: self
                .diagnostics
                .iter()
                .filter(|record| {
                    record
                        .path
                        .as_deref()
                        .is_none_or(|path| !path.starts_with("user/"))
                })
                .cloned()
                .collect(),
            reason_counts: BTreeMap::new(),
        };
        manifest.derive_reason_counts();
        manifest
    }

    pub(crate) fn admitted_view(&self, admit_project: bool) -> Self {
        let mut manifest = self.clone();
        if !admit_project {
            manifest.sources.clear();
            manifest
                .skills
                .retain(|skill| skill.scope == SkillScope::User);
            manifest.derive_reason_counts();
        }
        manifest
    }

    fn derive_reason_counts(&mut self) {
        self.reason_counts.clear();
        for diagnostic in &self.diagnostics {
            *self
                .reason_counts
                .entry(diagnostic.reason.clone())
                .or_insert(0) += 1;
        }
    }

    /// Canonical UTF-8 JSON encoding. Deterministic by construction: struct
    /// field order plus sorted `BTreeMap` keys.
    pub(crate) fn to_canonical_json(&self) -> String {
        serde_json::to_string(self).expect("manifest serialization is infallible")
    }

    /// Strict decode of a persisted canonical manifest string. Rejects
    /// duplicate keys, trailing data, unknown fields, unsupported versions,
    /// and every bound or internal-digest violation.
    pub(crate) fn from_canonical_json(text: &str) -> Result<Self, ManifestError> {
        reject_duplicate_keys(text)?;
        let manifest: Self = serde_json::from_str(text)
            .map_err(|error| ManifestError(format!("manifest does not parse: {error}")))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub(crate) fn validate(&self) -> Result<(), ManifestError> {
        if !matches!(self.version, LEGACY_MANIFEST_VERSION | MANIFEST_VERSION) {
            return Err(ManifestError(format!(
                "unsupported manifest version {}",
                self.version
            )));
        }
        if self.version == LEGACY_MANIFEST_VERSION && !self.skills.is_empty() {
            return Err(ManifestError(
                "legacy manifests cannot contain skills".to_owned(),
            ));
        }
        self.validate_sources()?;
        self.validate_skills()?;
        self.validate_diagnostics()?;
        Ok(())
    }

    fn validate_sources(&self) -> Result<(), ManifestError> {
        if self.sources.len() > MAX_EULER_MD_SOURCES {
            return Err(ManifestError(format!(
                "manifest lists {} sources; the limit is {MAX_EULER_MD_SOURCES}",
                self.sources.len()
            )));
        }
        let mut seen_paths = BTreeSet::new();
        let mut combined = 0usize;
        for source in &self.sources {
            validate_identity(&source.path)?;
            if !seen_paths.insert(source.path.as_str()) {
                return Err(ManifestError(format!(
                    "manifest lists {} twice",
                    source.path
                )));
            }
            if source.byte_len != source.content.len() as u64 {
                return Err(ManifestError(format!(
                    "recorded length for {} does not match its content",
                    source.path
                )));
            }
            if source.content.len() > MAX_EULER_MD_BYTES {
                return Err(ManifestError(format!(
                    "{} exceeds the per-file limit",
                    source.path
                )));
            }
            combined += source.content.len();
            if source.digest != source_digest_v1(&source.path, &source.content) {
                return Err(ManifestError(format!(
                    "recorded digest for {} does not match its content",
                    source.path
                )));
            }
        }
        if combined > MAX_COMBINED_EULER_MD_BYTES {
            return Err(ManifestError(
                "combined source content exceeds the aggregate limit".to_owned(),
            ));
        }
        Ok(())
    }

    fn validate_skills(&self) -> Result<(), ManifestError> {
        if self.skills.len() > MAX_SKILLS {
            return Err(ManifestError(format!(
                "manifest lists {} skills; the limit is {MAX_SKILLS}",
                self.skills.len()
            )));
        }
        let mut seen_names = BTreeSet::new();
        let mut seen_skill_paths = BTreeSet::new();
        let mut combined_skill_body = 0usize;
        let mut previous_name: Option<&str> = None;
        for skill in &self.skills {
            validate_skill_name(&skill.name)?;
            validate_skill_description(&skill.description)?;
            validate_identity(&skill.path)?;
            if !seen_names.insert(skill.name.as_str()) {
                return Err(ManifestError(format!(
                    "manifest lists skill {} twice",
                    skill.name
                )));
            }
            if !seen_skill_paths.insert(skill.path.as_str()) {
                return Err(ManifestError(format!(
                    "manifest lists skill path {} twice",
                    skill.path
                )));
            }
            if previous_name.is_some_and(|previous| previous >= skill.name.as_str()) {
                return Err(ManifestError(
                    "manifest skills are not in strict name order".to_owned(),
                ));
            }
            previous_name = Some(&skill.name);
            if skill.body_len != skill.body.len() as u64 {
                return Err(ManifestError(format!(
                    "recorded body length for skill {} does not match its body",
                    skill.name
                )));
            }
            if skill.body.len() > MAX_SKILL_BODY_BYTES {
                return Err(ManifestError(format!(
                    "skill {} exceeds the per-body limit",
                    skill.name
                )));
            }
            combined_skill_body = combined_skill_body.saturating_add(skill.body.len());
            if skill.body_digest
                != super::digest::skill_digest_v1(
                    skill.scope,
                    &skill.name,
                    &skill.path,
                    &skill.body,
                )
            {
                return Err(ManifestError(format!(
                    "recorded digest for skill {} does not match its body",
                    skill.name
                )));
            }
        }
        if combined_skill_body > MAX_COMBINED_SKILL_BODY_BYTES {
            return Err(ManifestError(
                "combined skill bodies exceed the aggregate limit".to_owned(),
            ));
        }
        Ok(())
    }

    fn validate_diagnostics(&self) -> Result<(), ManifestError> {
        if self.diagnostics.len() > MAX_MANIFEST_DIAGNOSTICS {
            return Err(ManifestError(format!(
                "manifest lists {} diagnostics; the limit is {MAX_MANIFEST_DIAGNOSTICS}",
                self.diagnostics.len()
            )));
        }
        let mut derived_counts: BTreeMap<String, u64> = BTreeMap::new();
        for diagnostic in &self.diagnostics {
            validate_reason_code(&diagnostic.reason)?;
            if let Some(path) = &diagnostic.path {
                validate_identity(path)?;
            }
            *derived_counts.entry(diagnostic.reason.clone()).or_default() += 1;
        }
        if derived_counts != self.reason_counts {
            return Err(ManifestError(
                "per-reason counts do not match the diagnostic records".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManifestError(pub String);

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ManifestError {}

pub(crate) fn validate_skill_name(name: &str) -> Result<(), ManifestError> {
    let bytes = name.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= MAX_SKILL_NAME_BYTES
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
        && !bytes.windows(2).any(|pair| pair == b"--")
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(ManifestError(format!(
            "skill name is not a normalized lowercase identifier: {name}"
        )))
    }
}

pub(crate) fn validate_skill_description(description: &str) -> Result<(), ManifestError> {
    let valid = !description.trim().is_empty()
        && description.len() <= MAX_SKILL_DESCRIPTION_BYTES
        && !description
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'));
    if valid {
        Ok(())
    } else {
        Err(ManifestError(
            "skill description is empty, too large, or contains control characters".to_owned(),
        ))
    }
}

/// A normalized project-root-relative identity: bounded UTF-8, `/`-separated,
/// no absolute prefix, no `.`/`..` traversal, no control characters.
pub(crate) fn validate_identity(path: &str) -> Result<(), ManifestError> {
    if path.is_empty() {
        return Err(ManifestError("source identity is empty".to_owned()));
    }
    if path.len() > MAX_IDENTITY_BYTES {
        return Err(ManifestError(
            "source identity exceeds the identity length bound".to_owned(),
        ));
    }
    if path.starts_with('/') || path.contains('\\') {
        return Err(ManifestError(format!(
            "source identity is not project-root-relative: {path}"
        )));
    }
    if path.chars().any(char::is_control) {
        return Err(ManifestError(
            "source identity contains control characters".to_owned(),
        ));
    }
    if path
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(ManifestError(format!(
            "source identity contains traversal or empty components: {path}"
        )));
    }
    Ok(())
}

/// Stable reason-code grammar shared by manifest records, snapshot payload
/// label fields, and diagnostic events: 1-64 bytes of ASCII lowercase,
/// digits, and underscores.
pub(crate) fn validate_reason_code(reason: &str) -> Result<(), ManifestError> {
    let valid = !reason.is_empty()
        && reason.len() <= 64
        && reason
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
    if valid {
        Ok(())
    } else {
        Err(ManifestError(format!(
            "diagnostic reason code is not a stable lowercase code: {reason}"
        )))
    }
}

/// Reject any duplicate object key anywhere in `text` (serde_json keeps the
/// last value silently, which would let two encodings share one digest), and
/// reject trailing data after the top-level value.
fn reject_duplicate_keys(text: &str) -> Result<(), ManifestError> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    DuplicateKeyCheck::deserialize(&mut deserializer)
        .map_err(|error| ManifestError(format!("manifest is not strict JSON: {error}")))?;
    deserializer
        .end()
        .map_err(|error| ManifestError(format!("manifest has trailing data: {error}")))?;
    Ok(())
}

struct DuplicateKeyCheck;

impl<'de> Deserialize<'de> for DuplicateKeyCheck {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(DuplicateKeyVisitor)
    }
}

struct DuplicateKeyVisitor;

impl<'de> Visitor<'de> for DuplicateKeyVisitor {
    type Value = DuplicateKeyCheck;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any strict JSON value")
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheck)
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheck)
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheck)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheck)
    }

    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheck)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheck)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq.next_element::<DuplicateKeyCheck>()?.is_some() {}
        Ok(DuplicateKeyCheck)
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom(format!("duplicate object key: {key}")));
            }
            map.next_value::<DuplicateKeyCheck>()?;
        }
        Ok(DuplicateKeyCheck)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(path: &str, content: &str) -> ManifestSource {
        ManifestSource {
            path: path.to_owned(),
            byte_len: content.len() as u64,
            digest: source_digest_v1(path, content),
            content: content.to_owned(),
        }
    }

    fn manifest() -> CandidateManifest {
        CandidateManifest {
            version: MANIFEST_VERSION,
            sources: vec![
                source("EULER.md", "root\n"),
                source("crates/EULER.md", "leaf"),
            ],
            skills: Vec::new(),
            diagnostics: vec![ManifestDiagnostic {
                reason: "source_too_large".to_owned(),
                path: Some("big/EULER.md".to_owned()),
                observed: Some(40_000),
            }],
            reason_counts: [("source_too_large".to_owned(), 1)].into_iter().collect(),
        }
    }

    #[test]
    fn canonical_round_trip_is_exact() {
        let manifest = manifest();
        let json = manifest.to_canonical_json();
        let decoded = CandidateManifest::from_canonical_json(&json).expect("round trip");
        assert_eq!(decoded, manifest);
        assert_eq!(decoded.to_canonical_json(), json);
    }

    #[test]
    fn duplicate_keys_reject() {
        let json = r#"{"version":1,"version":1,"sources":[],"diagnostics":[],"reason_counts":{}}"#;
        assert!(CandidateManifest::from_canonical_json(json).is_err());
        let nested = r#"{"version":1,"sources":[{"path":"EULER.md","path":"EULER.md","byte_len":0,"digest":"x","content":""}],"diagnostics":[],"reason_counts":{}}"#;
        assert!(CandidateManifest::from_canonical_json(nested).is_err());
    }

    #[test]
    fn trailing_data_rejects() {
        let json = format!("{} {{}}", manifest().to_canonical_json());
        assert!(CandidateManifest::from_canonical_json(&json).is_err());
    }

    #[test]
    fn unknown_fields_reject() {
        let json = r#"{"version":1,"sources":[],"diagnostics":[],"reason_counts":{},"extra":1}"#;
        assert!(CandidateManifest::from_canonical_json(json).is_err());
    }

    #[test]
    fn unsupported_version_rejects() {
        let json = r#"{"version":3,"sources":[],"diagnostics":[],"reason_counts":{}}"#;
        assert!(CandidateManifest::from_canonical_json(json).is_err());
    }

    #[test]
    fn digest_mismatch_rejects() {
        let mut manifest = manifest();
        manifest.sources[0].content.push('!');
        manifest.sources[0].byte_len += 1;
        let json = manifest.to_canonical_json();
        assert!(CandidateManifest::from_canonical_json(&json).is_err());
    }

    #[test]
    fn length_mismatch_rejects() {
        let mut manifest = manifest();
        manifest.sources[0].byte_len += 1;
        let json = manifest.to_canonical_json();
        assert!(CandidateManifest::from_canonical_json(&json).is_err());
    }

    #[test]
    fn traversal_identities_reject() {
        for path in ["/abs/EULER.md", "../EULER.md", "a//EULER.md", "a\\EULER.md"] {
            let mut manifest = manifest();
            manifest.sources[0] = source(path, "x");
            let json = manifest.to_canonical_json();
            assert!(
                CandidateManifest::from_canonical_json(&json).is_err(),
                "{path} must reject"
            );
        }
    }

    #[test]
    fn reason_count_mismatch_rejects() {
        let mut manifest = manifest();
        manifest.reason_counts.insert("io_error".to_owned(), 1);
        let json = manifest.to_canonical_json();
        assert!(CandidateManifest::from_canonical_json(&json).is_err());
    }

    #[test]
    fn per_file_bound_rejects_oversized_content() {
        let mut manifest = manifest();
        let oversized = "x".repeat(MAX_EULER_MD_BYTES + 1);
        manifest.sources[0] = source("EULER.md", &oversized);
        let json = manifest.to_canonical_json();
        assert!(CandidateManifest::from_canonical_json(&json).is_err());
    }
}
