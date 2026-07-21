//! Domain-separated, length-prefixed project-context digests.
//!
//! Every digest family carries its own versioned domain tag so a value from
//! one family can never verify in another (project-context contract,
//! "Snapshot, events, and replay"). Fields are length-prefixed with a
//! little-endian u64 so concatenation boundaries are unambiguous.

use sha2::{Digest, Sha256};
use std::path::Path;

/// Domain tag for the portable candidate digest over the canonical manifest
/// encoding (version 1).
pub(crate) const CANDIDATE_DOMAIN_V1: &str = "euler.project-context.candidate.v1";
/// Domain tag for one accepted source's content digest (version 1).
pub(crate) const SOURCE_DOMAIN_V1: &str = "euler.project-context.source.v1";
/// Domain tag for the rendered (core-framed) context digest (version 1).
pub(crate) const RENDERED_DOMAIN_V1: &str = "euler.project-context.rendered.v1";
/// Domain tag for the local workspace identity digest. The algorithm hashes
/// the raw `OsStr` bytes of the canonicalized workspace root on Unix hosts
/// with no lossy display conversion or Unicode normalization. A future host
/// requires a distinct algorithm version.
pub(crate) const WORKSPACE_IDENTITY_DOMAIN_V1: &str =
    "euler.project-context.workspace-identity.unix-raw-osstr.v1";

pub(crate) const WORKSPACE_IDENTITY_ALGORITHM: &str = "unix-raw-osstr";
pub(crate) const WORKSPACE_IDENTITY_VERSION: u32 = 1;

fn domain_separated_digest(domain: &str, fields: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0u8]);
    for field in fields {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    format!("{:x}", hasher.finalize())
}

/// Portable candidate digest over the canonical manifest JSON bytes.
pub(crate) fn candidate_digest_v1(manifest_json: &str) -> String {
    domain_separated_digest(CANDIDATE_DOMAIN_V1, &[manifest_json.as_bytes()])
}

/// Per-source digest over the normalized relative identity and the frozen
/// post-redaction content.
pub(crate) fn source_digest_v1(rel_path: &str, content: &str) -> String {
    domain_separated_digest(SOURCE_DOMAIN_V1, &[rel_path.as_bytes(), content.as_bytes()])
}

/// Digest of the exact rendered (core-framed) project-context bytes included
/// in a provider-neutral request.
pub(crate) fn rendered_digest_v1(rendered: &str) -> String {
    domain_separated_digest(RENDERED_DOMAIN_V1, &[rendered.as_bytes()])
}

/// Local workspace identity digest over the exact platform representation of
/// an already canonicalized workspace root. Unix-only by design: a future
/// host needs a distinct algorithm version and test vectors first.
#[cfg(unix)]
pub(crate) fn workspace_identity_digest_v1(canonical_root: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    domain_separated_digest(
        WORKSPACE_IDENTITY_DOMAIN_V1,
        &[canonical_root.as_os_str().as_bytes()],
    )
}

#[cfg(not(unix))]
pub(crate) fn workspace_identity_digest_v1(_canonical_root: &Path) -> String {
    // Non-Unix hosts have no ratified identity algorithm yet; returning a
    // digest here would silently invent one. Callers on such hosts never
    // build a project-context bootstrap (discovery is omitted), so this is
    // unreachable in practice; keep it explicit rather than panicking.
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_families_are_domain_separated() {
        let candidate = candidate_digest_v1("same-bytes");
        let rendered = rendered_digest_v1("same-bytes");
        assert_ne!(candidate, rendered);
        assert_eq!(candidate.len(), 64);
        assert!(candidate.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn length_prefix_disambiguates_field_boundaries() {
        // ("a", "bc") and ("ab", "c") concatenate identically without a
        // length prefix; the digest must distinguish them.
        assert_ne!(source_digest_v1("a", "bc"), source_digest_v1("ab", "c"));
    }

    #[test]
    fn digests_are_deterministic() {
        assert_eq!(
            candidate_digest_v1("manifest"),
            candidate_digest_v1("manifest")
        );
        assert_eq!(
            source_digest_v1("EULER.md", "content"),
            source_digest_v1("EULER.md", "content")
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_identity_depends_on_exact_path_bytes() {
        use std::path::PathBuf;
        let a = workspace_identity_digest_v1(&PathBuf::from("/tmp/workspace-a"));
        let b = workspace_identity_digest_v1(&PathBuf::from("/tmp/workspace-b"));
        assert_ne!(a, b);
        assert_eq!(
            a,
            workspace_identity_digest_v1(&PathBuf::from("/tmp/workspace-a"))
        );
    }
}
