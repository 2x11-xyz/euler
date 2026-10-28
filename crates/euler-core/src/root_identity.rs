//! Root object identity (issue #239): which directory object a session ran
//! in, recorded next to the path-only workspace identity of ADR 0017.
//!
//! The canonical path tells Euler where a session ran; it cannot tell a
//! directory from a different one later placed at the same path (a fresh
//! clone, a restored backup, `mv other ws`). Each `session.start` and
//! `session.resumed` record carries the opened canonical root's
//! `{ device, inode, birth }` in an optional field older binaries ignore.
//! Resume compares the latest recorded identity with the live root and yields
//! a verdict. The verdict never blocks or prompts: `Unverified` only disables
//! automatic replay (ADR 0023), and the session resumes as before.

use euler_event::{EventEnvelope, EventKind, JsonObject};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Optional payload key on `session.start` and `session.resumed`.
pub(crate) const ROOT_OBJECT_IDENTITY_FIELD: &str = "root_object_identity";

/// Filesystem identity of the opened canonical root directory. `birth` is
/// `None` when the filesystem does not report a creation time; inode numbers
/// alone can be reused after a delete, so a missing birth never verifies.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RootObjectIdentity {
    device: u64,
    inode: u64,
    birth: Option<RootBirthTime>,
}

/// Directory creation time since the Unix epoch.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RootBirthTime {
    secs: u64,
    nanos: u32,
}

/// Whether the live root is the directory the session last recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootIdentityVerdict {
    /// Same projected root path and same directory object.
    Verified,
    /// An accepted relocation (ADR 0017) moved the session after its root
    /// object was last recorded, so there is nothing to compare against.
    Relocated,
    Unverified {
        reason: RootUnverifiedReason,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootUnverifiedReason {
    /// The governing record predates the field or could not read its root.
    NotRecorded,
    /// The governing record carries a field this build cannot read.
    Malformed,
    /// The live root cannot be opened or its identity read.
    Unreadable,
    /// The recorded or live root has no birth time.
    BirthUnreported,
    /// The live root is at a different path or is a different directory.
    Changed,
}

impl RootObjectIdentity {
    /// Identity of the directory `root` resolves to, read from one open
    /// handle so every component describes the same directory object.
    pub(crate) fn read(root: &Path) -> Option<Self> {
        let metadata = std::fs::File::open(root).ok()?.metadata().ok()?;
        if !metadata.is_dir() {
            return None;
        }
        let (device, inode) = device_and_inode(&metadata)?;
        let birth = metadata
            .created()
            .ok()
            .and_then(|created| created.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| RootBirthTime {
                secs: since.as_secs(),
                nanos: since.subsec_nanos(),
            });
        Some(Self {
            device,
            inode,
            birth,
        })
    }
}

#[cfg(unix)]
fn device_and_inode(metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn device_and_inode(_metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// Record the live root's identity on a `session.start` or `session.resumed`
/// payload. An unreadable root leaves the field absent, which later resumes
/// read as `NotRecorded`.
pub(crate) fn insert_root_object_identity(payload: &mut JsonObject, root: &Path) {
    if let Some(identity) = RootObjectIdentity::read(root) {
        payload.insert(
            ROOT_OBJECT_IDENTITY_FIELD.to_owned(),
            serde_json::to_value(identity).expect("root object identity serializes"),
        );
    }
}

enum GoverningRoot {
    Recorded {
        identity: RootObjectIdentity,
        root: Option<String>,
    },
    NotRecorded,
    Malformed,
    Relocated,
}

/// Compare the root object identity governing `events` with `live_root`.
///
/// The latest `session.start` or `session.resumed` record governs, and a
/// later `project.context.relocated` supersedes it. A record without the
/// field (an older binary resumed the session) resets the governing identity
/// to unknown, because that resume may have run in a different directory.
pub(crate) fn root_identity_verdict(
    events: &[EventEnvelope],
    live_root: &Path,
) -> RootIdentityVerdict {
    let mut governing = GoverningRoot::NotRecorded;
    let mut projected_root: Option<String> = None;
    for event in events {
        let kind = event.kind.as_str();
        if kind == EventKind::SESSION_START {
            projected_root = event
                .payload
                .get("root")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
        } else if kind == EventKind::PROJECT_CONTEXT_RELOCATED {
            projected_root = event
                .payload
                .get("new_root")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            governing = GoverningRoot::Relocated;
            continue;
        } else if kind != EventKind::SESSION_RESUMED {
            continue;
        }
        governing = match event.payload.get(ROOT_OBJECT_IDENTITY_FIELD) {
            None => GoverningRoot::NotRecorded,
            Some(value) => match serde_json::from_value(value.clone()) {
                Ok(identity) => GoverningRoot::Recorded {
                    identity,
                    root: projected_root.clone(),
                },
                Err(_) => GoverningRoot::Malformed,
            },
        };
    }
    let unverified = |reason| RootIdentityVerdict::Unverified { reason };
    let (recorded, recorded_root) = match governing {
        GoverningRoot::Recorded { identity, root } => (identity, root),
        GoverningRoot::NotRecorded => return unverified(RootUnverifiedReason::NotRecorded),
        GoverningRoot::Malformed => return unverified(RootUnverifiedReason::Malformed),
        GoverningRoot::Relocated => return RootIdentityVerdict::Relocated,
    };
    let Some(live) = RootObjectIdentity::read(live_root) else {
        return unverified(RootUnverifiedReason::Unreadable);
    };
    // ADR 0017 already compares paths for project-context sessions; sessions
    // without a snapshot have no path check, and a moved directory keeps its
    // object identity, so the projected root path must match as well.
    let live_root_display = crate::session_root::session_root_for_event(live_root);
    if recorded_root.as_deref() != Some(live_root_display.as_str())
        || recorded.device != live.device
        || recorded.inode != live.inode
    {
        return unverified(RootUnverifiedReason::Changed);
    }
    match (recorded.birth, live.birth) {
        (Some(recorded_birth), Some(live_birth)) if recorded_birth == live_birth => {
            RootIdentityVerdict::Verified
        }
        (Some(_), Some(_)) => unverified(RootUnverifiedReason::Changed),
        _ => unverified(RootUnverifiedReason::BirthUnreported),
    }
}
