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
use crate::provenance::{LogScrubStats, ProvenanceWriter, ProvenanceWriterError};
use std::fs::{self};
use std::io::{self, Write};
use std::path::Path;
use thiserror::Error;
use ulid::Ulid;

/// Agent id recorded on the `secret.scrubbed` audit event for a post-close
/// scrub — the live path records the session's own agent instead.
pub const SCRUB_AGENT: &str = "euler-scrub";

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

/// Post-close scrub: acquire the session lock by opening a fresh writer, then
/// remove `secrets` from every surface of `session_dir`.
pub fn scrub_closed_session(
    session_dir: &Path,
    session_id: &str,
    surfaces: ScrubSurfaces<'_>,
    secrets: &[String],
) -> Result<ScrubReport, ScrubError> {
    let writer = ProvenanceWriter::new(session_dir.join("events.jsonl"))?;
    let stats = writer.scrub_and_audit(secrets, surfaces.workspace_root, session_id, SCRUB_AGENT)?;
    drop(writer);
    let mut report = ScrubReport::from_log(stats);
    finish_non_log_surfaces(session_dir, surfaces, secrets, &mut report)?;
    Ok(report)
}

/// Scrub the surfaces that live outside the append-only log — the session-title
/// sidecar and the store index. Shared by the closed and live paths; the log
/// itself is scrubbed by the caller (a fresh writer, or the live session's).
pub fn finish_non_log_surfaces(
    session_dir: &Path,
    surfaces: ScrubSurfaces<'_>,
    secrets: &[String],
    report: &mut ScrubReport,
) -> Result<(), ScrubError> {
    report.sidecar_scrubbed = scrub_text_file(&session_dir.join("session.json"), secrets)?;
    if let Some(index_path) = surfaces.index_path {
        report.index_scrubbed = scrub_text_file(index_path, secrets)?;
    }
    Ok(())
}

/// Build a live-path report from the log stats produced by the session's own
/// writer, so `Session::scrub_live` and the closed path share one report shape.
pub fn report_from_log_stats(stats: LogScrubStats) -> ScrubReport {
    ScrubReport::from_log(stats)
}

/// Atomically replace every secret occurrence in a private text file (session
/// sidecar / index). Returns whether anything changed. Missing file is a no-op.
/// Written 0600 via temp+rename+fsync, matching the store's other sidecars.
fn scrub_text_file(path: &Path, secrets: &[String]) -> Result<bool, ScrubError> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let (scrubbed, replacements) = crate::redaction::scrub_secrets_in_text(&content, secrets);
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
