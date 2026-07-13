//! Credential scrub (issue #100): the counterpart to faithful provenance.
//!
//! Euler never silently redacts model cognition, tool-call arguments, or user
//! messages — but a live credential can still land in a faithful payload. When
//! that happens euler warns rather than corrupts (see `secrets.md`), and this
//! module is the explicit, user-initiated removal that the warning offers.
//!
//! One surface-sweeping engine, two entry points:
//! - **live** `/scrub` runs against the session's own writer mid-turn;
//! - **post-close** `euler scrub` runs against a closed session directory.
//!
//! Both remove the value from every session-owned persistent surface — the
//! append-only log, externalized blobs, workspace pre-image checkpoints,
//! extension artifacts and private state, and the session-title sidecar — then
//! append a `secret.scrubbed` audit event (counts only, never the value).
//! Already-exported, copied, terminal-scrollback, or pushed data cannot be
//! recalled.

use crate::home::{containing_dir, private_open_options, set_file_mode_0600, sync_dir};
use crate::provenance::{ProvenanceWriter, ProvenanceWriterError};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;
use ulid::Ulid;

/// Agent id recorded on the `secret.scrubbed` audit event for a post-close
/// scrub — the live path records the session's own agent instead.
pub const SCRUB_AGENT: &str = "euler-scrub";

/// Explicit scrub values shorter than this are rejected: substring-replacing a
/// 1-3 char value would mangle unrelated content far more than it protects.
pub const MIN_SCRUB_VALUE_LEN: usize = 4;

/// Prepare caller-supplied secrets for scrubbing: drop values too short to
/// scrub safely, and sort longest-first so a longer secret is removed before
/// any shorter secret it contains (overlap-safe, order-independent).
pub fn prepare_secrets(secrets: &[String]) -> Vec<String> {
    let mut prepared: Vec<String> = secrets
        .iter()
        .filter(|value| value.len() >= MIN_SCRUB_VALUE_LEN)
        .cloned()
        .collect();
    prepared.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    prepared.dedup();
    prepared
}

/// What a scrub removed, across every surface. `anything_scrubbed()` is false
/// when the value was not present anywhere (a no-op scrub).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScrubReport {
    pub events_rewritten: usize,
    pub blobs_rewritten: usize,
    pub checkpoints_rewritten: usize,
    pub extension_artifacts_rewritten: usize,
    pub extension_state_files_rewritten: usize,
    pub replacements: usize,
    pub sidecar_scrubbed: bool,
    pub audit_event_id: Option<String>,
}

impl ScrubReport {
    pub fn anything_scrubbed(&self) -> bool {
        self.replacements > 0
    }

    /// One-line human summary for the slash command / CLI. Never contains the
    /// value; only counts and the un-recall caveat.
    pub fn summary_line(&self) -> String {
        if !self.anything_scrubbed() {
            return "scrub: nothing matched — no occurrences on any surface".to_owned();
        }
        let mut parts = Vec::new();
        parts.push(format!(
            "{} occurrence{}",
            self.replacements,
            plural(self.replacements),
        ));
        if self.events_rewritten > 0 {
            parts.push(format!(
                "{} event{}",
                self.events_rewritten,
                plural(self.events_rewritten),
            ));
        }
        if self.blobs_rewritten > 0 {
            parts.push(format!(
                "{} blob{}",
                self.blobs_rewritten,
                plural(self.blobs_rewritten)
            ));
        }
        if self.checkpoints_rewritten > 0 {
            parts.push(format!(
                "{} checkpoint{}",
                self.checkpoints_rewritten,
                plural(self.checkpoints_rewritten)
            ));
        }
        if self.extension_artifacts_rewritten > 0 {
            parts.push(format!(
                "{} extension artifact{}",
                self.extension_artifacts_rewritten,
                plural(self.extension_artifacts_rewritten)
            ));
        }
        if self.extension_state_files_rewritten > 0 {
            parts.push(format!(
                "{} extension state file{}",
                self.extension_state_files_rewritten,
                plural(self.extension_state_files_rewritten)
            ));
        }
        if self.sidecar_scrubbed {
            parts.push("session title".to_owned());
        }
        format!(
            "scrubbed {} — already-exported, copied, terminal-scrollback, or pushed data cannot be recalled",
            parts.join(", ")
        )
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

/// Where a scrub reaches beyond the append-only log. All optional: a scrub of a
/// bare session directory with no known workspace or store still cleans the log
/// and sidecar.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScrubSurfaces<'a> {
    /// Workspace root for `.euler/checkpoints` pre-images (from the session's
    /// recorded root). `None` skips checkpoint scrubbing.
    pub workspace_root: Option<&'a Path>,
}

/// Post-close scrub: remove `secrets` from every surface of `session_dir`.
///
/// The writer lock is acquired before any surface changes, so this path cannot
/// mutate an active session and then discover the ownership conflict. The
/// success audit is appended only after every scrubbed surface is durable.
pub fn scrub_closed_session(
    session_dir: &Path,
    session_id: &str,
    surfaces: ScrubSurfaces<'_>,
    secrets: &[String],
) -> Result<ScrubReport, ScrubError> {
    let secrets = prepare_secrets(secrets);
    if secrets.is_empty() {
        return Ok(ScrubReport::default());
    }
    let writer = ProvenanceWriter::new(session_dir.join("events.jsonl"))?;
    let report =
        writer.scrub_and_audit(&secrets, surfaces.workspace_root, session_id, SCRUB_AGENT)?;
    drop(writer);
    Ok(report)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct FileScrub {
    pub changed: bool,
    pub secret_replacements: usize,
}

/// Structurally scrub one JSON state file and update any content-addressed
/// references changed by the same transaction. Malformed files are scrubbed as
/// bytes: corruption must not become a reason to leave a credential behind.
pub(crate) fn scrub_json_file(
    path: &Path,
    secrets: &[String],
    references: &[(String, String)],
) -> io::Result<FileScrub> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(FileScrub::default()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("scrub surface is not a regular file: {}", path.display()),
        ));
    }
    let original = fs::read(path)?;
    let (scrubbed, secret_replacements) =
        match serde_json::from_slice::<serde_json::Value>(&original) {
            Ok(mut value) => {
                let replacements = crate::redaction::scrub_secrets_in_value(&mut value, secrets);
                let references_changed = replace_references_in_value(&mut value, references);
                if replacements == 0 && !references_changed {
                    return Ok(FileScrub::default());
                }
                let mut bytes = serde_json::to_vec_pretty(&value).map_err(io::Error::other)?;
                bytes.push(b'\n');
                (bytes, replacements)
            }
            Err(_) => {
                let (mut bytes, replacements) =
                    crate::redaction::scrub_secrets_in_bytes(&original, secrets);
                for (from, to) in references {
                    bytes =
                        crate::redaction::replace_bytes(&bytes, from.as_bytes(), to.as_bytes()).0;
                }
                (bytes, replacements)
            }
        };
    if scrubbed == original {
        return Ok(FileScrub {
            changed: false,
            secret_replacements,
        });
    }
    write_private_atomic(path, &scrubbed)?;
    Ok(FileScrub {
        changed: true,
        secret_replacements,
    })
}

fn replace_references_in_value(
    value: &mut serde_json::Value,
    references: &[(String, String)],
) -> bool {
    match value {
        serde_json::Value::String(text) => {
            let mut changed = false;
            for (from, to) in references {
                if text.contains(from) {
                    *text = text.replace(from, to);
                    changed = true;
                }
            }
            changed
        }
        serde_json::Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= replace_references_in_value(item, references);
            }
            changed
        }
        serde_json::Value::Object(map) => {
            let mut changed = false;
            let entries = std::mem::take(map);
            for (mut key, mut value) in entries {
                for (from, to) in references {
                    if key.contains(from) {
                        key = key.replace(from, to);
                        changed = true;
                    }
                }
                changed |= replace_references_in_value(&mut value, references);
                let key = crate::redaction::unique_json_key(map, key);
                map.insert(key, value);
            }
            changed
        }
        _ => false,
    }
}

pub(crate) fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("scrub");
    let temp_path: PathBuf = path.with_file_name(format!(".{file_name}.{}.scrub.tmp", Ulid::new()));
    let result = (|| {
        let mut file = private_open_options()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        set_file_mode_0600(&file)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_data()?;
        drop(file);
        fs::rename(&temp_path, path)?;
        sync_dir(containing_dir(path))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

#[derive(Debug, Error)]
pub enum ScrubError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Writer(#[from] ProvenanceWriterError),
    #[error("failed to reconcile the live session after scrub: {0}")]
    Reconcile(String),
}

#[cfg(test)]
#[path = "scrub_test.rs"]
mod scrub_test;
