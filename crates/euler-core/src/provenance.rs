use euler_event::{EventEnvelope, EventKind};
use euler_sdk::{event_wake::EventWakeRegistry, EventWakeError, EventWakeRegistration};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;
use thiserror::Error;

pub const DEFAULT_BLOB_THRESHOLD: usize = 8 * 1024;
pub const DEFAULT_PROVENANCE_QUERY_EVENT_LIMIT: usize = 256;
pub const DEFAULT_PROVENANCE_QUERY_SCAN_LIMIT: usize = 1024;
pub const DEFAULT_PROVENANCE_QUERY_BLOB_BYTE_LIMIT: usize = 1024 * 1024;

pub type EventId = String;

#[derive(Debug)]
pub struct ProvenanceWriter {
    log_path: PathBuf,
    blob_dir: PathBuf,
    threshold: usize,
    policy: PersistPolicy,
    append_lock: Mutex<AppendState>,
    event_wakes: EventWakeRegistry,
    _lock: SessionLock,
}

#[derive(Debug)]
struct AppendState {
    durable_tail: Option<EventId>,
    pending_resume_marker: Option<EventEnvelope>,
}

impl ProvenanceWriter {
    pub fn new(log_path: impl Into<PathBuf>) -> Result<Self, ProvenanceWriterError> {
        let log_path = log_path.into();
        let session_dir = log_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        Self::with_threshold(log_path, session_dir.join("blobs"), DEFAULT_BLOB_THRESHOLD)
    }

    pub fn with_threshold(
        log_path: PathBuf,
        blob_dir: PathBuf,
        threshold: usize,
    ) -> Result<Self, ProvenanceWriterError> {
        let lock = SessionLock::acquire(&log_path)?;
        let durable_tail = match latest_accepted_event_id(&log_path) {
            Ok(tail) => tail,
            // A directory at the log path opens with an empty tail on purpose:
            // failure-path tests (and the failure surface they pin) expect
            // writer construction to succeed and the APPEND to fail with the
            // real I/O error. See session_loop.rs failed-switch coverage.
            Err(EventWakeError::Io(source)) if source.kind() == io::ErrorKind::IsADirectory => None,
            Err(EventWakeError::Io(source)) => return Err(ProvenanceWriterError::Io(source)),
            Err(EventWakeError::InvalidLine { source }) => {
                return Err(ProvenanceWriterError::InvalidLine { source });
            }
            Err(EventWakeError::ReceiverLimit) => {
                return Err(ProvenanceWriterError::Io(io::Error::other(
                    "event wake receiver limit",
                )));
            }
        };
        Ok(Self {
            log_path,
            blob_dir,
            threshold,
            policy: PersistPolicy,
            append_lock: Mutex::new(AppendState {
                durable_tail,
                pending_resume_marker: None,
            }),
            event_wakes: EventWakeRegistry::default(),
            _lock: lock,
        })
    }

    pub fn append(&self, events: &[EventEnvelope]) -> io::Result<()> {
        let mut append_guard = recover_mutex(&self.append_lock);
        self.append_locked(&mut append_guard, events).map(|_| ())
    }

    /// Append a batch parented from this writer's durable tail.
    /// The builder runs under the append lock and must only construct events:
    /// no writer/session/host callbacks, I/O, or blocking work. Builder panic
    /// appends nothing and leaves the tail unchanged. Persisted non-semantic
    /// events are chained linearly; closed-list semantic parents are preserved.
    pub fn append_parented(
        &self,
        build: impl FnOnce(Option<EventId>) -> Vec<EventEnvelope>,
    ) -> io::Result<Vec<EventEnvelope>> {
        let mut append_guard = recover_mutex(&self.append_lock);
        let tail_at_acquisition = append_guard.durable_tail.clone();
        let mut events = build(tail_at_acquisition.clone());
        self.assign_batch_parents(&mut events, tail_at_acquisition);
        self.append_locked(&mut append_guard, &events)?;
        Ok(events
            .into_iter()
            .filter(|event| self.policy.classify(event.kind.as_str()) == PersistDecision::Persist)
            .collect())
    }

    pub fn durable_tail(&self) -> Option<EventId> {
        recover_mutex(&self.append_lock).durable_tail.clone()
    }

    /// Arm one log-only resume marker to precede the next durable append.
    /// Keeping it under the append lock makes the boundary cover every writer
    /// client (turns, control actions, extensions, and child agents) without
    /// making the marker part of the session bus or the conversation chain.
    pub(crate) fn arm_resume_marker(&self, marker: EventEnvelope) -> io::Result<()> {
        if marker.kind.as_str() != EventKind::SESSION_RESUMED {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pending resume marker has the wrong event kind",
            ));
        }
        let mut state = recover_mutex(&self.append_lock);
        if state.pending_resume_marker.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a resume marker is already pending",
            ));
        }
        if marker.parent != state.durable_tail {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "resume marker does not parent the durable tail",
            ));
        }
        state.pending_resume_marker = Some(marker);
        Ok(())
    }

    /// Scrub `secrets` out of the durable ledger (issue #100).
    ///
    /// Rewrites the accepted prefix of the log in place: every occurrence in an
    /// event payload (including inline `projection_blob` compaction state) is
    /// replaced with the scrub marker, externalized blobs holding a secret are
    /// rewritten under a fresh content hash and re-pointed, and — when
    /// `workspace_root` is known — `file.change` pre-image checkpoints are
    /// rewritten the same way. Event ids, timestamps, kinds, and ordering are
    /// preserved, so the only tail movement is the appended `secret.scrubbed`
    /// audit event (counts only, never the value).
    ///
    /// Held under the append lock so no concurrent append races. Atomic: the
    /// new log is fsynced and renamed over the old; new blobs are durable
    /// before the log commit; superseded blobs are removed after it. Returns a
    /// no-op result (`audit_event_id: None`) when no surface held a secret.
    pub fn scrub_and_audit(
        &self,
        secrets: &[String],
        workspace_root: Option<&Path>,
        session_id: &str,
        agent: &str,
    ) -> io::Result<LogScrubStats> {
        let mut durable_tail = recover_mutex(&self.append_lock);

        let content = match fs::read_to_string(&self.log_path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error),
        };
        let mut events = accepted_prefix_lines(&content)
            .into_iter()
            .map(EventEnvelope::from_json_line)
            .collect::<Result<Vec<_>, _>>()
            .map_err(io::Error::other)?;

        let mut pass = ScrubPass::default();
        for event in &mut events {
            self.scrub_one_event(event, secrets, workspace_root, &mut pass)?;
        }
        if !pass.stats.touched_anything() {
            return Ok(pass.stats);
        }

        let audit = EventEnvelope::new(
            session_id,
            agent,
            events.last().map(|event| event.id.clone()),
            EventKind::new(EventKind::SECRET_SCRUBBED),
            scrub_audit_payload(secrets.len(), &pass.stats),
        );
        pass.stats.audit_event_id = Some(audit.id.clone());
        events.push(audit);

        // New blobs are durable before the log commit; superseded ones removed
        // after, so a crash never strands a secret-bearing blob the committed
        // log still points at.
        for (path, bytes) in &pass.new_blobs {
            write_blob_durable(path, bytes)?;
        }
        self.commit_scrubbed_log(&events)?;
        for path in &pass.retired_blobs {
            let _ = fs::remove_file(path);
        }
        if !pass.new_blobs.is_empty() {
            let _ = sync_dir(&self.blob_dir);
        }

        *durable_tail = events.last().map(|event| event.id.clone());
        self.event_wakes.notify_advanced();
        Ok(pass.stats)
    }

    /// Scrub one event's payload, externalized blobs, and (when the workspace
    /// root is known) `file.change` pre-image checkpoint in place, recording
    /// deferred blob side effects and counts into `pass`.
    fn scrub_one_event(
        &self,
        event: &mut EventEnvelope,
        secrets: &[String],
        workspace_root: Option<&Path>,
        pass: &mut ScrubPass,
    ) -> io::Result<()> {
        let mut replacements = 0;
        for value in event.payload.values_mut() {
            replacements += crate::redaction::scrub_secrets_in_value(value, secrets);
        }
        if replacements > 0 {
            pass.stats.events_rewritten += 1;
            pass.stats.replacements += replacements;
        }

        for (field, hash) in event.blobs.clone() {
            let Some(new_hash) = self.scrub_one_blob(&hash, secrets, pass) else {
                continue;
            };
            event.blobs.insert(field.clone(), new_hash.clone());
            event
                .payload
                .insert(field.clone(), format!("blob:{new_hash}").into());
            pass.stats.blobs_rewritten += 1;
        }

        if let Some(root) = workspace_root {
            if event.kind.as_str() == EventKind::FILE_CHANGE {
                if let Some(old_hash) = event
                    .payload
                    .get("pre_image_blob")
                    .and_then(serde_json::Value::as_str)
                    .filter(|hash| !hash.is_empty())
                    .map(str::to_owned)
                {
                    if let Some(new_hash) =
                        crate::checkpoints::scrub_pre_image(root, &old_hash, secrets)?
                    {
                        event
                            .payload
                            .insert("pre_image_blob".to_owned(), new_hash.into());
                        pass.stats.checkpoints_rewritten += 1;
                    }
                }
            }
        }
        Ok(())
    }

    /// Rewrite an externalized blob that holds a secret, returning the new
    /// content hash to re-point at. `None` when the blob is missing, non-UTF-8,
    /// or holds no secret. Same content hashes to the same value, so a blob
    /// shared by several events is read and rewritten once (cached in `pass`).
    fn scrub_one_blob(&self, hash: &str, secrets: &[String], pass: &mut ScrubPass) -> Option<String> {
        if let Some(new_hash) = pass.blob_rewrites.get(hash) {
            return Some(new_hash.clone());
        }
        let path = self.blob_dir.join(hash);
        let text = String::from_utf8(fs::read(&path).ok()?).ok()?;
        let (scrubbed, count) = crate::redaction::scrub_secrets_in_text(&text, secrets);
        if count == 0 {
            return None;
        }
        pass.stats.replacements += count;
        let new_hash = hash_bytes(scrubbed.as_bytes());
        pass.new_blobs
            .push((self.blob_dir.join(&new_hash), scrubbed.into_bytes()));
        if new_hash != hash {
            pass.retired_blobs.push(path);
        }
        pass.blob_rewrites.insert(hash.to_owned(), new_hash.clone());
        Some(new_hash)
    }

    /// Atomically replace the log with `events`: fsync a temp file and rename
    /// it over the log, then fsync the directory.
    fn commit_scrubbed_log(&self, events: &[EventEnvelope]) -> io::Result<()> {
        let temp_path = temp_path_with_suffix(&self.log_path, ".scrub.tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp_path)?;
        for event in events {
            let line = event.to_json_line().map_err(io::Error::other)?;
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        file.sync_data()?;
        drop(file);
        fs::rename(&temp_path, &self.log_path)?;
        sync_dir(containing_dir(&self.log_path))
    }

    fn append_locked(
        &self,
        state: &mut AppendState,
        events: &[EventEnvelope],
    ) -> io::Result<usize> {
        let started = Instant::now();
        let persisted_events = events
            .iter()
            .filter(|event| self.policy.classify(event.kind.as_str()) == PersistDecision::Persist)
            .collect::<Vec<_>>();
        if persisted_events.is_empty() {
            return Ok(0);
        }
        let pending_resume_marker = state.pending_resume_marker.clone();
        let session_id = pending_resume_marker
            .as_ref()
            .or_else(|| persisted_events.first().copied())
            .map_or("unknown", |event| event.session.as_str());
        let log_dir = containing_dir(&self.log_path);
        create_dir_all_durable(log_dir)?;
        create_dir_all_durable(&self.blob_dir)?;
        let event_count = persisted_events.len() + usize::from(pending_resume_marker.is_some());
        let events = pending_resume_marker
            .iter()
            .chain(persisted_events)
            .map(|event| self.externalize_large_payloads(event))
            .collect::<io::Result<Vec<_>>>()?;
        let new_tail = events.last().map(|event| event.id.clone());
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        let mut bytes = 0_u64;
        for event in events {
            let line = event.to_json_line().map_err(io::Error::other)?;
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
            bytes = bytes.saturating_add(line.len() as u64).saturating_add(1);
        }
        file.flush()?;
        file.sync_data()?;
        // Keep a newly created log name durable; this dir fsync is cheap
        // relative to the log fsync and harmless for later appends.
        sync_dir(log_dir)?;
        // Not transactional: if an I/O failure occurs after bytes reached the
        // file but before this point, the in-memory tail stays at the
        // pre-append value and the writer surfaces the error. Callers treat
        // append failure as session-fatal, so the stale-tail window is never
        // built upon.
        state.durable_tail = new_tail;
        state.pending_resume_marker = None;
        self.event_wakes.notify_advanced();
        crate::diagnostics::provenance_append_end(
            session_id,
            persisted_events_count(event_count),
            bytes,
            elapsed_ms(started),
        );
        Ok(event_count)
    }

    pub fn open_event_wake(&self) -> Result<EventWakeRegistration, EventWakeError> {
        let append_guard = recover_mutex(&self.append_lock);
        let baseline_event_id = append_guard.durable_tail.clone();
        self.event_wakes.open(baseline_event_id)
    }

    pub(crate) fn log_path(&self) -> &Path {
        &self.log_path
    }

    fn externalize_large_payloads(&self, event: &EventEnvelope) -> io::Result<EventEnvelope> {
        let mut event = event.clone();
        for &field in externalized_payload_fields(event.kind.as_str()) {
            let Some(value) = event
                .payload
                .get(field)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
            else {
                continue;
            };
            if value.len() <= self.threshold {
                continue;
            }
            let hash = hash_bytes(value.as_bytes());
            let path = self.blob_dir.join(&hash);
            write_blob_durable(&path, value.as_bytes())?;
            event
                .payload
                .insert(field.to_owned(), format!("blob:{hash}").into());
            event.blobs.insert(field.to_owned(), hash);
        }
        Ok(event)
    }

    fn assign_batch_parents(
        &self,
        events: &mut [EventEnvelope],
        mut linear_parent: Option<EventId>,
    ) {
        for event in events
            .iter_mut()
            .filter(|event| self.policy.classify(event.kind.as_str()) == PersistDecision::Persist)
        {
            if !has_explicit_semantic_parent(event) {
                event.parent.clone_from(&linear_parent);
            }
            linear_parent = Some(event.id.clone());
        }
    }
}

fn externalized_payload_fields(kind: &str) -> &'static [&'static str] {
    match kind {
        EventKind::TOOL_RESULT => &["output"],
        EventKind::PATCH_PROPOSED | EventKind::PATCH_APPLIED => &["old", "new"],
        _ => &[],
    }
}

fn has_explicit_semantic_parent(event: &EventEnvelope) -> bool {
    if event.parent.is_none() {
        return false;
    }
    match event.kind.as_str() {
        EventKind::PERMISSION_DECISION | EventKind::TOOL_RESULT | EventKind::AGENT_RESULT => true,
        EventKind::ERROR => {
            event
                .payload
                .get("source")
                .and_then(serde_json::Value::as_str)
                == Some("extension")
        }
        _ => false,
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn persisted_events_count(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

impl Drop for ProvenanceWriter {
    fn drop(&mut self) {
        self.event_wakes.close_all();
    }
}

fn latest_accepted_event_id(path: &Path) -> Result<Option<String>, EventWakeError> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut latest = None;
    for line in accepted_prefix_lines(&content) {
        let event = EventEnvelope::from_json_line(line)
            .map_err(|source| EventWakeError::InvalidLine { source })?;
        latest = Some(event.id);
    }
    Ok(latest)
}

pub fn read_provenance(path: impl AsRef<Path>) -> Result<Vec<EventEnvelope>, ProvenanceReadError> {
    let path = path.as_ref();
    let content = fs::read_to_string(path)?;
    let blob_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("blobs");
    let mut events = Vec::new();

    for line in accepted_prefix_lines(&content) {
        match EventEnvelope::from_json_line(line) {
            Ok(event) => events.push(rehydrate_blobs(event, &blob_dir)?),
            Err(source) => return Err(ProvenanceReadError::InvalidLine { source }),
        }
    }

    Ok(events)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvenanceQuery {
    /// Stream-position cursor. Writer-created `EventEnvelope` ids are unique;
    /// this query does not build an unbounded duplicate-id index.
    pub after_event_id: Option<String>,
    pub kinds: Vec<String>,
    pub limit: usize,
    pub scan_limit: usize,
    pub include_blob_fields: bool,
    pub blob_byte_limit: usize,
}

impl ProvenanceQuery {
    pub fn new(limit: usize) -> Self {
        Self {
            after_event_id: None,
            kinds: Vec::new(),
            limit,
            scan_limit: DEFAULT_PROVENANCE_QUERY_SCAN_LIMIT,
            include_blob_fields: false,
            blob_byte_limit: DEFAULT_PROVENANCE_QUERY_BLOB_BYTE_LIMIT,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProvenancePage {
    pub events: Vec<EventEnvelope>,
    pub applied_limit: usize,
    pub applied_scan_limit: usize,
    pub scanned_events: usize,
    pub watermark_event_id: Option<String>,
    pub next_after_event_id: Option<String>,
    pub truncated: bool,
}

pub fn query_provenance(
    path: impl AsRef<Path>,
    query: ProvenanceQuery,
) -> Result<ProvenancePage, ProvenanceQueryError> {
    if query.limit == 0 {
        return Err(ProvenanceQueryError::InvalidLimit);
    }
    if query.scan_limit == 0 {
        return Err(ProvenanceQueryError::InvalidScanLimit);
    }

    let path = path.as_ref();
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let blob_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("blobs");
    let applied_limit = query.limit.min(DEFAULT_PROVENANCE_QUERY_EVENT_LIMIT);
    let applied_scan_limit = query.scan_limit.min(DEFAULT_PROVENANCE_QUERY_SCAN_LIMIT);
    let mut events = Vec::new();
    let mut scanned_events = 0;
    let mut watermark_event_id = None;
    let mut cursor_seen = query.after_event_id.is_none();
    let mut blob_budget = BlobExpansionBudget::capped(query.blob_byte_limit);
    let cursor = query.after_event_id.as_deref();
    let mut truncated = false;
    let mut next_after_event_id = None;

    while let Some(line) = next_accepted_query_line(&mut reader, &mut line)? {
        if line.trim().is_empty() {
            continue;
        }
        let event = EventEnvelope::from_json_line(line)
            .map_err(|source| ProvenanceQueryError::InvalidLine { source })?;
        if !cursor_seen {
            if Some(event.id.as_str()) == cursor {
                cursor_seen = true;
                watermark_event_id = Some(event.id);
            }
            continue;
        }
        if scanned_events == applied_scan_limit {
            truncated = true;
            next_after_event_id.clone_from(&watermark_event_id);
            break;
        }
        let matches_kind = query.matches_kind(event.kind.as_str());
        if matches_kind && events.len() == applied_limit {
            truncated = true;
            next_after_event_id.clone_from(&watermark_event_id);
            break;
        }
        let event_id = event.id.clone();
        scanned_events += 1;
        watermark_event_id = Some(event_id);
        if !matches_kind {
            continue;
        }

        let event = if query.include_blob_fields {
            expand_blobs(event, &blob_dir, &mut blob_budget)?
        } else {
            event
        };
        events.push(event);
    }

    if !cursor_seen {
        let event_id = query.after_event_id.expect("cursor was requested");
        return Err(ProvenanceQueryError::CursorNotFound { event_id });
    }

    Ok(ProvenancePage {
        events,
        applied_limit,
        applied_scan_limit,
        scanned_events,
        watermark_event_id,
        next_after_event_id: truncated.then_some(next_after_event_id).flatten(),
        truncated,
    })
}

fn next_accepted_query_line<'a>(
    reader: &mut impl BufRead,
    buffer: &'a mut Vec<u8>,
) -> Result<Option<&'a str>, ProvenanceQueryError> {
    buffer.clear();
    let read = reader.read_until(b'\n', buffer)?;
    if read == 0 || !buffer.ends_with(b"\n") {
        return Ok(None);
    }
    let line = buffer.strip_suffix(b"\n").expect("checked newline suffix");
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    std::str::from_utf8(line).map(Some).map_err(|source| {
        ProvenanceQueryError::Io(io::Error::new(io::ErrorKind::InvalidData, source))
    })
}

impl ProvenanceQuery {
    fn matches_kind(&self, event_kind: &str) -> bool {
        self.kinds.is_empty() || self.kinds.iter().any(|kind| kind == event_kind)
    }
}

pub(crate) fn accepted_prefix_lines(content: &str) -> Vec<&str> {
    accepted_prefix_line_iter(content).collect()
}

fn accepted_prefix_line_iter(content: &str) -> impl Iterator<Item = &str> {
    let prefix = if content.ends_with('\n') {
        content
    } else {
        content
            .rsplit_once('\n')
            .map_or("", |(accepted, _torn)| accepted)
    };
    prefix.lines().filter(|line| !line.trim().is_empty())
}

fn rehydrate_blobs(
    event: EventEnvelope,
    blob_dir: &Path,
) -> Result<EventEnvelope, ProvenanceReadError> {
    let mut budget = BlobExpansionBudget::unbounded();
    expand_blobs(event, blob_dir, &mut budget).map_err(ProvenanceReadError::from_blob_expansion)
}

fn expand_blobs(
    mut event: EventEnvelope,
    blob_dir: &Path,
    budget: &mut BlobExpansionBudget,
) -> Result<EventEnvelope, BlobExpansionError> {
    let refs = event
        .blobs
        .iter()
        .map(|(field, hash)| (field.clone(), hash.clone()))
        .collect::<Vec<_>>();

    for (field, hash) in refs {
        let path = blob_dir.join(&hash);
        let len = blob_len(&path, &field, &hash)?;
        budget.reserve(len, &field, &hash, &path)?;
        let bytes = read_blob_bounded(&path, &field, &hash, len)?;
        if hash_bytes(&bytes) != hash {
            return Err(BlobExpansionError::BlobHashMismatch { field, hash, path });
        }
        let content = String::from_utf8(bytes).map_err(|source| {
            BlobExpansionError::Io(io::Error::new(io::ErrorKind::InvalidData, source))
        })?;
        event.payload.insert(field.clone(), content.into());
        event.blobs.remove(&field);
    }

    Ok(event)
}

fn read_blob_bounded(
    path: &Path,
    field: &str,
    hash: &str,
    len: usize,
) -> Result<Vec<u8>, BlobExpansionError> {
    let read_limit = len
        .checked_add(1)
        .ok_or_else(|| BlobExpansionError::BlobChanged {
            field: field.to_owned(),
            hash: hash.to_owned(),
            path: path.to_path_buf(),
        })?;
    let mut file = File::open(path).map_err(|source| match source.kind() {
        io::ErrorKind::NotFound => BlobExpansionError::MissingBlob {
            field: field.to_owned(),
            hash: hash.to_owned(),
            path: path.to_path_buf(),
        },
        _ => BlobExpansionError::Io(source),
    })?;
    let mut bytes = Vec::with_capacity(len);
    Read::by_ref(&mut file)
        .take(u64::try_from(read_limit).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(BlobExpansionError::Io)?;
    if bytes.len() > len {
        return Err(BlobExpansionError::BlobChanged {
            field: field.to_owned(),
            hash: hash.to_owned(),
            path: path.to_path_buf(),
        });
    }
    Ok(bytes)
}

fn blob_len(path: &Path, field: &str, hash: &str) -> Result<usize, BlobExpansionError> {
    let len = fs::metadata(path)
        .map_err(|source| match source.kind() {
            io::ErrorKind::NotFound => BlobExpansionError::MissingBlob {
                field: field.to_owned(),
                hash: hash.to_owned(),
                path: path.to_path_buf(),
            },
            _ => BlobExpansionError::Io(source),
        })?
        .len();
    Ok(usize::try_from(len).unwrap_or(usize::MAX))
}

#[derive(Debug)]
struct BlobExpansionBudget {
    limit: Option<usize>,
    used: usize,
}

impl BlobExpansionBudget {
    fn capped(limit: usize) -> Self {
        Self {
            limit: Some(limit),
            used: 0,
        }
    }

    fn unbounded() -> Self {
        Self {
            limit: None,
            used: 0,
        }
    }

    fn reserve(
        &mut self,
        bytes: usize,
        field: &str,
        hash: &str,
        path: &Path,
    ) -> Result<(), BlobExpansionError> {
        let requested = self.used.saturating_add(bytes);
        if self.limit.is_some_and(|limit| requested > limit) {
            return Err(BlobExpansionError::BlobByteLimitExceeded {
                limit: self.limit.expect("checked above"),
                requested,
                field: field.to_owned(),
                hash: hash.to_owned(),
                path: path.to_path_buf(),
            });
        }
        self.used = requested;
        Ok(())
    }
}

#[derive(Debug)]
enum BlobExpansionError {
    Io(io::Error),
    MissingBlob {
        field: String,
        hash: String,
        path: PathBuf,
    },
    BlobHashMismatch {
        field: String,
        hash: String,
        path: PathBuf,
    },
    BlobChanged {
        field: String,
        hash: String,
        path: PathBuf,
    },
    BlobByteLimitExceeded {
        limit: usize,
        requested: usize,
        field: String,
        hash: String,
        path: PathBuf,
    },
}

fn write_blob_durable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if path.exists() && fs::read(path)? == bytes {
        let file = OpenOptions::new().read(true).open(path)?;
        file.sync_data()?;
        return Ok(());
    }

    let temp_path = temp_path_with_suffix(path, ".tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&temp_path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_data()?;
    drop(file);
    fs::rename(&temp_path, path)?;
    sync_dir(containing_dir(path))?;
    Ok(())
}

#[derive(Debug)]
struct SessionLock {
    path: PathBuf,
    pid: u32,
}

impl SessionLock {
    fn acquire(log_path: &Path) -> Result<Self, ProvenanceWriterError> {
        let path = lock_path_for(log_path);
        create_dir_all_durable(containing_dir(&path))?;
        let pid = std::process::id();
        loop {
            match Self::create(&path, pid) {
                Ok(lock) => return Ok(lock),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let holder = read_lock_pid(&path);
                    let Some(holder_pid) = holder else {
                        return Err(ProvenanceWriterError::SessionLocked { path, pid: holder });
                    };
                    if pid_is_alive(holder_pid) {
                        return Err(ProvenanceWriterError::SessionLocked { path, pid: holder });
                    }
                    reclaim_stale_lock(&path, pid, holder)?;
                }
                Err(source) => return Err(ProvenanceWriterError::Io(source)),
            }
        }
    }

    fn create(path: &Path, pid: u32) -> io::Result<Self> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(pid.to_string().as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_data()?;
        sync_dir(containing_dir(path))?;
        Ok(Self {
            path: path.to_path_buf(),
            pid,
        })
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        if read_lock_pid(&self.path) == Some(self.pid) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn lock_path_for(log_path: &Path) -> PathBuf {
    let mut lock_path: OsString = log_path.as_os_str().to_owned();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}

fn reclaim_stale_lock(
    path: &Path,
    pid: u32,
    stale_pid: Option<u32>,
) -> Result<(), ProvenanceWriterError> {
    let reclaim_path = temp_path_with_suffix(path, &format!(".{pid}.reclaim"));
    let _ = fs::remove_file(&reclaim_path);

    // Rename claims the specific lock file atomically. This prevents two
    // reclaimers from both deleting by path after one has already created
    // a fresh lock for the same session.
    match fs::rename(path, &reclaim_path) {
        Ok(()) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(ProvenanceWriterError::Io(source)),
    }

    let reclaimed_pid = read_lock_pid(&reclaim_path);
    if reclaimed_pid != stale_pid {
        match fs::rename(&reclaim_path, path) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => return Err(ProvenanceWriterError::Io(source)),
        }
        return Err(ProvenanceWriterError::SessionLocked {
            path: path.to_path_buf(),
            pid: reclaimed_pid,
        });
    }

    fs::remove_file(&reclaim_path)?;
    Ok(())
}

fn read_lock_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(target_os = "linux")]
fn pid_is_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(target_os = "linux"))]
fn pid_is_alive(_pid: u32) -> bool {
    true
}

fn temp_path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut temp_path: OsString = path.as_os_str().to_owned();
    temp_path.push(suffix);
    PathBuf::from(temp_path)
}

fn containing_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn create_dir_all_durable(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    sync_dir(containing_dir(path))
}

fn recover_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> io::Result<()> {
    // std exposes no portable directory fsync on non-Unix platforms.
    Ok(())
}

#[derive(Debug, Error)]
pub enum ProvenanceWriterError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid provenance line: {source}")]
    InvalidLine {
        #[source]
        source: serde_json::Error,
    },
    #[error("provenance session is already locked at {}", path.display())]
    SessionLocked { path: PathBuf, pid: Option<u32> },
}

#[derive(Debug, Error)]
pub enum ProvenanceReadError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid provenance line: {source}")]
    InvalidLine {
        #[source]
        source: serde_json::Error,
    },
    #[error("missing provenance blob for field {field}: {hash} at {}", path.display())]
    MissingBlob {
        field: String,
        hash: String,
        path: PathBuf,
    },
    #[error("provenance blob hash mismatch for field {field}: {hash} at {}", path.display())]
    BlobHashMismatch {
        field: String,
        hash: String,
        path: PathBuf,
    },
}

impl ProvenanceReadError {
    fn from_blob_expansion(error: BlobExpansionError) -> Self {
        match error {
            BlobExpansionError::Io(source) => Self::Io(source),
            BlobExpansionError::MissingBlob { field, hash, path } => {
                Self::MissingBlob { field, hash, path }
            }
            BlobExpansionError::BlobHashMismatch { field, hash, path } => {
                Self::BlobHashMismatch { field, hash, path }
            }
            BlobExpansionError::BlobChanged { field, hash, path } => {
                Self::BlobHashMismatch { field, hash, path }
            }
            BlobExpansionError::BlobByteLimitExceeded { .. } => Self::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected blob byte limit while reading provenance",
            )),
        }
    }
}

#[derive(Debug, Error)]
pub enum ProvenanceQueryError {
    #[error("provenance query limit must be nonzero")]
    InvalidLimit,
    #[error("provenance query scan limit must be nonzero")]
    InvalidScanLimit,
    #[error("provenance query cursor event id was not found in accepted prefix: {event_id}")]
    CursorNotFound { event_id: String },
    #[error("invalid provenance line: {source}")]
    InvalidLine {
        #[source]
        source: serde_json::Error,
    },
    #[error("missing provenance blob for field {field}: {hash} at {}", path.display())]
    MissingBlob {
        field: String,
        hash: String,
        path: PathBuf,
    },
    #[error("provenance blob hash mismatch for field {field}: {hash} at {}", path.display())]
    BlobHashMismatch {
        field: String,
        hash: String,
        path: PathBuf,
    },
    #[error("provenance blob changed while reading field {field}: {hash} at {}", path.display())]
    BlobChanged {
        field: String,
        hash: String,
        path: PathBuf,
    },
    #[error(
        "provenance query blob byte limit exceeded for field {field}: requested {requested} bytes with limit {limit} ({hash} at {})",
        path.display()
    )]
    BlobByteLimitExceeded {
        limit: usize,
        requested: usize,
        field: String,
        hash: String,
        path: PathBuf,
    },
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<BlobExpansionError> for ProvenanceQueryError {
    fn from(error: BlobExpansionError) -> Self {
        match error {
            BlobExpansionError::Io(source) => Self::Io(source),
            BlobExpansionError::MissingBlob { field, hash, path } => {
                Self::MissingBlob { field, hash, path }
            }
            BlobExpansionError::BlobHashMismatch { field, hash, path } => {
                Self::BlobHashMismatch { field, hash, path }
            }
            BlobExpansionError::BlobChanged { field, hash, path } => {
                Self::BlobChanged { field, hash, path }
            }
            BlobExpansionError::BlobByteLimitExceeded {
                limit,
                requested,
                field,
                hash,
                path,
            } => Self::BlobByteLimitExceeded {
                limit,
                requested,
                field,
                hash,
                path,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PersistPolicy;

impl PersistPolicy {
    pub fn classify(&self, kind: &str) -> PersistDecision {
        match kind {
            EventKind::MODEL_DELTA => PersistDecision::RuntimeOnly,
            EventKind::FILE_CHANGE => PersistDecision::Persist,
            _ => PersistDecision::Persist,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistDecision {
    Persist,
    RuntimeOnly,
}

/// Whether an event kind is runtime-only and must never be persisted or
/// exported (e.g. `model.delta`; see `docs/contracts/persistence.md`).
///
/// Delegates to the same [`PersistPolicy::classify`] match used for
/// provenance writes so callers (like `/export`) cannot drift from the
/// persistence classifier.
pub fn event_is_runtime_only(kind: &str) -> bool {
    PersistPolicy.classify(kind) == PersistDecision::RuntimeOnly
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

/// Per-surface tally from [`ProvenanceWriter::scrub_and_audit`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LogScrubStats {
    /// Distinct events whose payload was rewritten.
    pub events_rewritten: usize,
    /// Externalized blobs rewritten under a fresh content hash.
    pub blobs_rewritten: usize,
    /// Workspace pre-image checkpoints rewritten.
    pub checkpoints_rewritten: usize,
    /// Total substring replacements across payloads and blobs.
    pub replacements: usize,
    /// Id of the appended `secret.scrubbed` audit event, or `None` for a no-op.
    pub audit_event_id: Option<String>,
}

impl LogScrubStats {
    fn touched_anything(&self) -> bool {
        self.events_rewritten > 0 || self.blobs_rewritten > 0 || self.checkpoints_rewritten > 0
    }
}

/// Mutable accumulator threaded through a scrub over the log's events: running
/// counts plus deferred blob side effects (written/removed around the atomic
/// log commit) and a per-hash rewrite cache so a shared blob is handled once.
#[derive(Default)]
struct ScrubPass {
    stats: LogScrubStats,
    new_blobs: Vec<(PathBuf, Vec<u8>)>,
    retired_blobs: Vec<PathBuf>,
    blob_rewrites: std::collections::HashMap<String, String>,
}

fn scrub_audit_payload(values: usize, stats: &LogScrubStats) -> euler_event::JsonObject {
    let mut surfaces = serde_json::Map::new();
    surfaces.insert("events".to_owned(), stats.events_rewritten.into());
    surfaces.insert("blobs".to_owned(), stats.blobs_rewritten.into());
    surfaces.insert("checkpoints".to_owned(), stats.checkpoints_rewritten.into());
    let mut payload = serde_json::Map::new();
    payload.insert("values".to_owned(), values.into());
    payload.insert("replacements".to_owned(), stats.replacements.into());
    payload.insert("surfaces".to_owned(), surfaces.into());
    payload.insert(
        "note".to_owned(),
        "already-exported or pushed copies cannot be recalled".into(),
    );
    payload
}

#[cfg(test)]
#[path = "provenance_test.rs"]
mod provenance_test;
