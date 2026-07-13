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
//! Both remove the value from every persistent surface — the append-only log,
//! externalized blobs, workspace pre-image checkpoints, the session-title
//! sidecar, and the store index — and append a `secret.scrubbed` audit event
//! (counts only, never the value). Already-exported or pushed copies cannot be
//! recalled; the audit note and the report both say so.

use crate::home::{containing_dir, private_open_options, set_file_mode_0600, sync_dir};
use crate::provenance::{LogScrubStats, NonLogScrub, ProvenanceWriter, ProvenanceWriterError};
use std::fs::{self};
use std::io::{self, Write};
use std::path::Path;
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
    pub replacements: usize,
    pub sidecar_scrubbed: bool,
    pub index_scrubbed: bool,
    pub audit_event_id: Option<String>,
}

impl ScrubReport {
    fn from_log(stats: LogScrubStats) -> Self {
        Self {
            events_rewritten: stats.events_rewritten,
            blobs_rewritten: stats.blobs_rewritten,
            checkpoints_rewritten: stats.checkpoints_rewritten,
            replacements: stats.replacements,
            sidecar_scrubbed: false,
            index_scrubbed: false,
            audit_event_id: stats.audit_event_id,
        }
    }

    pub fn anything_scrubbed(&self) -> bool {
        self.replacements > 0
            || self.blobs_rewritten > 0
            || self.checkpoints_rewritten > 0
            || self.sidecar_scrubbed
            || self.index_scrubbed
    }

    /// One-line human summary for the slash command / CLI. Never contains the
    /// value; only counts and the un-recall caveat.
    pub fn summary_line(&self) -> String {
        if !self.anything_scrubbed() {
            return "scrub: nothing matched — no occurrences on any surface".to_owned();
        }
        let mut parts = Vec::new();
        if self.replacements > 0 {
            parts.push(format!(
                "{} occurrence{} across {} event{}",
                self.replacements,
                plural(self.replacements),
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
        if self.sidecar_scrubbed {
            parts.push("session title".to_owned());
        }
        if self.index_scrubbed {
            parts.push("store index".to_owned());
        }
        format!(
            "scrubbed {} — already-exported or pushed copies cannot be recalled",
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
    /// The store's `index.jsonl`, whose entry carries the session name. `None`
    /// skips index scrubbing (e.g. a detached session directory).
    pub index_path: Option<&'a Path>,
}

/// Post-close scrub: remove `secrets` from every surface of `session_dir`.
///
/// The non-log surfaces (sidecar, index) are scrubbed FIRST so their outcome is
/// known before the audit commits — a failure there aborts before any audit
/// claims success, and a secret found only there is still recorded. The log
/// rewrite + `secret.scrubbed` audit then commit last, reflecting all surfaces.
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
    let non_log = scrub_non_log_surfaces(session_dir, surfaces.index_path, &secrets)?;
    let writer = ProvenanceWriter::new(session_dir.join("events.jsonl"))?;
    let stats = writer.scrub_and_audit(
        &secrets,
        surfaces.workspace_root,
        session_id,
        SCRUB_AGENT,
        non_log,
    )?;
    drop(writer);
    Ok(report_from(stats, non_log))
}

/// Scrub the surfaces that live outside the append-only log — the session-title
/// sidecar and the store index — structurally (as JSON), returning what each
/// held. Shared by the closed and live paths; the log itself is scrubbed by the
/// caller (a fresh writer, or the live session's).
pub fn scrub_non_log_surfaces(
    session_dir: &Path,
    index_path: Option<&Path>,
    secrets: &[String],
) -> Result<NonLogScrub, ScrubError> {
    let sidecar = scrub_json_file(
        &session_dir.join("session.json"),
        secrets,
        JsonShape::Object,
    )?;
    let index = match index_path {
        Some(path) => scrub_json_file(path, secrets, JsonShape::Lines)?,
        None => false,
    };
    Ok(NonLogScrub { sidecar, index })
}

/// Build a report from the log stats and the non-log outcome, so the live and
/// closed paths share one report shape.
pub fn report_from(stats: LogScrubStats, non_log: NonLogScrub) -> ScrubReport {
    let mut report = ScrubReport::from_log(stats);
    report.sidecar_scrubbed = non_log.sidecar;
    report.index_scrubbed = non_log.index;
    report
}

/// Whether a JSON surface is a single object (`session.json`) or one JSON value
/// per line (`index.jsonl`).
#[derive(Clone, Copy)]
enum JsonShape {
    Object,
    Lines,
}

/// Scrub secrets out of a private JSON surface STRUCTURALLY: parse, replace in
/// string leaves, re-serialize. Unlike a raw substring replace, a secret
/// containing JSON metacharacters (`"`, `\`) can never corrupt the file. Returns
/// whether anything changed; a missing or unparseable file is a no-op. Written
/// 0600 via temp+rename+fsync, matching the store's other sidecars.
fn scrub_json_file(path: &Path, secrets: &[String], shape: JsonShape) -> Result<bool, ScrubError> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let (scrubbed, replacements) = match shape {
        JsonShape::Object => scrub_json_object(&content, secrets),
        JsonShape::Lines => scrub_json_lines(&content, secrets),
    };
    if replacements == 0 {
        return Ok(false);
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("scrub");
    let temp_path = path.with_file_name(format!(".{file_name}.{}.scrub.tmp", Ulid::new()));
    {
        let mut file = private_open_options()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        set_file_mode_0600(&file)?;
        file.write_all(scrubbed.as_bytes())?;
        file.flush()?;
        file.sync_data()?;
    }
    fs::rename(&temp_path, path)?;
    sync_dir(containing_dir(path))?;
    Ok(true)
}

/// Scrub a single JSON object (session.json), preserving pretty formatting.
fn scrub_json_object(content: &str, secrets: &[String]) -> (String, usize) {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(content) else {
        // Malformed (should not happen for euler-written sidecars): fall back to
        // a raw substring scrub rather than leave a secret behind.
        return crate::redaction::scrub_secrets_in_text(content, secrets);
    };
    let replacements = crate::redaction::scrub_secrets_in_value(&mut value, secrets);
    if replacements == 0 {
        return (content.to_owned(), 0);
    }
    match serde_json::to_string_pretty(&value) {
        Ok(mut serialized) => {
            serialized.push('\n');
            (serialized, replacements)
        }
        Err(_) => crate::redaction::scrub_secrets_in_text(content, secrets),
    }
}

/// Scrub JSONL (index.jsonl) line by line, each parsed as a JSON value.
fn scrub_json_lines(content: &str, secrets: &[String]) -> (String, usize) {
    let mut total = 0;
    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        if line.trim().is_empty() {
            out.push('\n');
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(mut value) => {
                total += crate::redaction::scrub_secrets_in_value(&mut value, secrets);
                match serde_json::to_string(&value) {
                    Ok(serialized) => out.push_str(&serialized),
                    Err(_) => {
                        let (serialized, count) =
                            crate::redaction::scrub_secrets_in_text(line, secrets);
                        total += count;
                        out.push_str(&serialized);
                    }
                }
            }
            Err(_) => {
                let (serialized, count) = crate::redaction::scrub_secrets_in_text(line, secrets);
                total += count;
                out.push_str(&serialized);
            }
        }
        out.push('\n');
    }
    (out, total)
}

#[derive(Debug, Error)]
pub enum ScrubError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Writer(#[from] ProvenanceWriterError),
}

#[cfg(test)]
#[path = "scrub_test.rs"]
mod scrub_test;
