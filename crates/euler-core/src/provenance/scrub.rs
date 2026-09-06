use super::{
    containing_dir, hash_bytes, nul_offset_in_line, numbered_accepted_prefix_lines, recover_mutex,
    sync_dir, unresolved_append_fence, ProvenanceWriter,
};
use crate::redaction::{
    scrub_byte_needles, scrub_event_payload, scrub_secret_byte_needles, scrub_secrets_in_bytes,
    SCRUBBED,
};
use crate::scrub::{scrub_json_file, write_private_atomic, ScrubReport};
use euler_event::{EventEnvelope, EventKind};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const EXTENSIONS_DIR: &str = "extensions";
const ARTIFACTS_DIR: &str = "artifacts";

impl ProvenanceWriter {
    /// Remove explicit values from every session-owned persistent surface.
    /// The existing log is rewritten before old content-addressed files are
    /// retired, but the success audit is appended last. A cleanup failure can
    /// therefore leave a partially scrubbed session, never a false success
    /// record; a retry sweeps orphaned files as well as referenced ones.
    pub(crate) fn scrub_and_audit(
        &self,
        secrets: &[String],
        workspace_root: Option<&Path>,
        session_id: &str,
        agent: &str,
    ) -> io::Result<ScrubReport> {
        let mut append_state = recover_mutex(&self.append_lock);
        if append_state.unresolved_append.is_some() {
            return Err(unresolved_append_fence());
        }
        let raw_len = match fs::metadata(&self.log_path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error),
        };
        if raw_len != append_state.durable_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "provenance log has bytes beyond its confirmed durable tail",
            ));
        }
        let content = match fs::read_to_string(&self.log_path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error),
        };
        let mut events = Vec::new();
        for line in numbered_accepted_prefix_lines(&content) {
            if let Some(nul) = nul_offset_in_line(line.text) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "provenance log is corrupted at line {} (byte offset {}): unexpected NUL bytes",
                        line.number,
                        line.offset + nul
                    ),
                ));
            }
            events.push(EventEnvelope::from_json_line(line.text).map_err(io::Error::other)?);
        }
        let mut pass = ScrubPass::default();
        let protect_response_protocol = self.response_protocol_is_valid_for_scrub(&events)?;
        let cross_boundary = self.cross_boundary_response_redactions(&events, secrets)?;
        pass.report.replacements += cross_boundary.values().sum::<usize>();
        pass.cross_boundary_responses = cross_boundary.into_keys().collect();
        self.prepare_collapsed_response_blobs(&events, &mut pass)?;

        for event in &mut events {
            self.scrub_event(
                event,
                secrets,
                workspace_root,
                protect_response_protocol,
                &mut pass,
            )?;
        }
        self.sweep_content_stores(secrets, &mut pass)?;
        // State projections can carry the rewritten artifact pointers. Make
        // their new content-addressed targets durable before updating those
        // projections, so a later failure never leaves a pointer dangling.
        pass.persist_new_files()?;
        self.scrub_session_state(secrets, &mut pass)?;

        if !pass.report.anything_scrubbed() {
            return Ok(pass.report);
        }

        if pass.log_changed {
            self.commit_scrubbed_log(&events)?;
            append_state.durable_len = fs::metadata(&self.log_path)?.len();
        }
        pass.retire_old_files()?;

        let audit = EventEnvelope::new(
            session_id,
            agent,
            append_state.durable_tail.clone(),
            EventKind::new(EventKind::SECRET_SCRUBBED),
            scrub_audit_payload(secrets.len(), &pass.report),
        );
        pass.report.audit_event_id = Some(audit.id.clone());
        self.append_locked(&mut append_state, std::slice::from_ref(&audit))?;
        Ok(pass.report)
    }

    fn scrub_event(
        &self,
        event: &mut EventEnvelope,
        secrets: &[String],
        workspace_root: Option<&Path>,
        protect_response_protocol: bool,
        pass: &mut ScrubPass,
    ) -> io::Result<()> {
        let mut changed = false;

        // Artifact routing fields must remain intact until the backing file is
        // resolved and rehashed. The general payload walk follows so user
        // content in every field is still scrubbed.
        if event.kind.as_str() == EventKind::EXTENSION_ARTIFACT {
            changed |= self.scrub_extension_artifact(event, secrets, pass)?;
        }
        let collapsed_response = pass.is_collapsed_response_chunk(event);
        changed |= pass.collapse_cross_boundary_response_chunk(event);
        changed |= scrub_inline_response_chunk(event, secrets, collapsed_response, pass)?;
        let replacements = scrub_event_payload(event, secrets, protect_response_protocol);
        if replacements > 0 {
            pass.report.replacements += replacements;
            changed = true;
        }

        let mut response_content_bytes = if event.kind.as_str()
            == EventKind::ASSISTANT_RESPONSE_CHUNK
            && !event.blobs.contains_key("content")
        {
            event
                .payload
                .get("content")
                .and_then(serde_json::Value::as_str)
                .map(str::len)
        } else {
            None
        };

        for (field, old_hash) in event.blobs.clone() {
            let Some(rewrite) = pass.rewrite_cached_file(
                StoreKind::Blob,
                old_hash.clone(),
                self.blob_dir.join(&old_hash),
                secrets,
            )?
            else {
                continue;
            };
            event.blobs.insert(field.clone(), rewrite.new_hash.clone());
            event
                .payload
                .insert(field.clone(), format!("blob:{}", rewrite.new_hash).into());
            if event.kind.as_str() == EventKind::ASSISTANT_RESPONSE_CHUNK && field == "content" {
                response_content_bytes = Some(rewrite.scrubbed.len());
            }
            changed = true;
        }

        changed |= pass.reaccount_response_bytes(event, response_content_bytes);

        if event.kind.as_str() == EventKind::FILE_CHANGE {
            if let (Some(root), Some(old_hash)) = (
                workspace_root,
                event
                    .payload
                    .get("pre_image_blob")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            ) {
                let path = crate::checkpoints::checkpoint_blob_path(root, &old_hash);
                if let Some(rewrite) =
                    pass.rewrite_cached_file(StoreKind::Checkpoint, old_hash, path, secrets)?
                {
                    event
                        .payload
                        .insert("pre_image_blob".to_owned(), rewrite.new_hash.into());
                    changed = true;
                }
            }
        }

        if changed {
            pass.report.events_rewritten += 1;
            pass.log_changed = true;
        }
        Ok(())
    }

    fn cross_boundary_response_redactions(
        &self,
        events: &[EventEnvelope],
        secrets: &[String],
    ) -> io::Result<HashMap<String, usize>> {
        let needles = scrub_secret_byte_needles(secrets);
        if needles.is_empty() {
            return Ok(HashMap::new());
        }
        let suffix_bound = needles
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
            .saturating_sub(1);
        let mut streams = HashMap::<String, ResponseSeamState>::new();
        let mut redactions = HashMap::new();
        for event in events {
            if matches!(
                event.kind.as_str(),
                EventKind::MODEL_RESULT | EventKind::ERROR
            ) {
                if let Some(response_id) = event
                    .payload
                    .get("response_id")
                    .and_then(serde_json::Value::as_str)
                {
                    if let Some(stream) = streams.remove(response_id) {
                        record_collapsed_response(&mut redactions, response_id, stream);
                    }
                }
                continue;
            }
            if event.kind.as_str() != EventKind::ASSISTANT_RESPONSE_CHUNK {
                continue;
            }
            let Some(response_id) = event.payload.get("response_id").and_then(|id| id.as_str())
            else {
                continue;
            };
            let content = self.response_chunk_content(event)?;
            let stream = streams.entry(response_id.to_owned()).or_default();
            let (scrubbed_chunk, replacements) = scrub_byte_needles(&content, &needles);
            stream.replacements = stream.replacements.saturating_add(replacements);
            stream.collapse |=
                scrubbed_chunk.len() > crate::assistant_response::MAX_RESPONSE_CHUNK_BYTES;
            let seam_matches = crossing_needle_count(&stream.suffix, &content, &needles);
            stream.replacements = stream.replacements.saturating_add(seam_matches);
            stream.collapse |= seam_matches > 0;
            retain_bounded_suffix(&mut stream.suffix, &content, suffix_bound);
        }
        for (response_id, stream) in streams {
            record_collapsed_response(&mut redactions, &response_id, stream);
        }
        Ok(redactions)
    }

    fn response_chunk_content(&self, event: &EventEnvelope) -> io::Result<Vec<u8>> {
        let Some(hash) = event.blobs.get("content") else {
            let content = event
                .payload
                .get("content")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| invalid_data("response chunk has no content"))?;
            if content.len() > crate::assistant_response::MAX_RESPONSE_CHUNK_BYTES {
                return Err(invalid_data("response chunk exceeds its byte bound"));
            }
            return Ok(content.as_bytes().to_vec());
        };
        let path = self.blob_dir.join(hash);
        if fs::metadata(&path)?.len()
            > u64::try_from(crate::assistant_response::MAX_RESPONSE_CHUNK_BYTES).unwrap_or(u64::MAX)
        {
            return Err(invalid_data("response chunk exceeds its byte bound"));
        }
        let bytes = fs::read(path)?;
        if hash_bytes(&bytes) != *hash {
            return Err(invalid_data("response chunk blob hash mismatch"));
        }
        std::str::from_utf8(&bytes)
            .map_err(|_| invalid_data("response chunk blob is not valid UTF-8"))?;
        Ok(bytes)
    }

    fn response_protocol_is_valid_for_scrub(&self, events: &[EventEnvelope]) -> io::Result<bool> {
        let mut projection =
            crate::assistant_response::AssistantResponseProjection::validation_only();
        for event in events {
            if event.kind.as_str() == EventKind::ASSISTANT_RESPONSE_CHUNK
                && event.blobs.contains_key("content")
            {
                let mut rehydrated = event.clone();
                let content = String::from_utf8(self.response_chunk_content(event)?)
                    .map_err(|_| invalid_data("response chunk blob is not valid UTF-8"))?;
                rehydrated
                    .payload
                    .insert("content".to_owned(), content.into());
                if projection.ingest(&rehydrated).is_err() {
                    return Ok(false);
                }
            } else if projection.ingest(event).is_err() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn prepare_collapsed_response_blobs(
        &self,
        events: &[EventEnvelope],
        pass: &mut ScrubPass,
    ) -> io::Result<()> {
        let hashes = events
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::ASSISTANT_RESPONSE_CHUNK)
            .filter(|event| {
                event
                    .payload
                    .get("response_id")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|response_id| pass.cross_boundary_responses.contains(response_id))
            })
            .filter_map(|event| event.blobs.get("content").cloned())
            .collect::<BTreeSet<_>>();
        for hash in hashes {
            pass.force_blob_scrub(hash.clone(), self.blob_dir.join(hash))?;
        }
        Ok(())
    }

    fn scrub_extension_artifact(
        &self,
        event: &mut EventEnvelope,
        secrets: &[String],
        pass: &mut ScrubPass,
    ) -> io::Result<bool> {
        let extension_id = required_payload_string(event, "extension_id")?;
        if !euler_sdk::valid_extension_identifier(&extension_id) {
            return Err(invalid_data(
                "extension artifact has an invalid extension id",
            ));
        }
        let old_hash = required_payload_string(event, "sha256")?;
        if !is_sha256_hex(&old_hash) {
            return Err(invalid_data("extension artifact has an invalid hash"));
        }
        let old_relative_path = required_payload_string(event, "path")?;
        let suffix = format!("{EXTENSIONS_DIR}/{extension_id}/{ARTIFACTS_DIR}/{old_hash}");
        if old_relative_path != suffix && !old_relative_path.ends_with(&format!("/{suffix}")) {
            return Err(invalid_data(
                "extension artifact path does not match its extension and hash",
            ));
        }
        let artifact_path = containing_dir(&self.log_path)
            .join(EXTENSIONS_DIR)
            .join(&extension_id)
            .join(ARTIFACTS_DIR)
            .join(&old_hash);
        let cache_key = format!("{extension_id}:{old_hash}");
        let Some(rewrite) = pass.rewrite_cached_file(
            StoreKind::ExtensionArtifact,
            cache_key,
            artifact_path,
            secrets,
        )?
        else {
            return Ok(false);
        };
        let new_relative_path = old_relative_path
            .strip_suffix(&old_hash)
            .map(|prefix| format!("{prefix}{}", rewrite.new_hash))
            .ok_or_else(|| invalid_data("extension artifact path has no hash suffix"))?;
        event
            .payload
            .insert("sha256".to_owned(), rewrite.new_hash.clone().into());
        event
            .payload
            .insert("path".to_owned(), new_relative_path.clone().into());
        event
            .payload
            .insert("byte_len".to_owned(), rewrite.scrubbed.len().into());
        pass.reference_rewrites
            .push((old_relative_path, new_relative_path));
        pass.reference_rewrites.push((old_hash, rewrite.new_hash));
        Ok(true)
    }

    fn sweep_content_stores(&self, secrets: &[String], pass: &mut ScrubPass) -> io::Result<()> {
        pass.sweep_dir(StoreKind::Blob, &self.blob_dir, secrets)?;
        // Checkpoints are workspace-global and can be referenced by another
        // session. Only hashes cited by this session are in scope; sweeping
        // the shared directory would mutate unrelated provenance.
        let extensions_dir = containing_dir(&self.log_path).join(EXTENSIONS_DIR);
        for extension_dir in child_dirs(&extensions_dir)? {
            pass.sweep_dir(
                StoreKind::ExtensionArtifact,
                &extension_dir.join(ARTIFACTS_DIR),
                secrets,
            )?;
        }
        Ok(())
    }

    fn scrub_session_state(&self, secrets: &[String], pass: &mut ScrubPass) -> io::Result<()> {
        pass.reference_rewrites
            .sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        pass.reference_rewrites.dedup();

        let session_dir = containing_dir(&self.log_path);
        let sidecar = scrub_json_file(
            &session_dir.join("session.json"),
            secrets,
            &pass.reference_rewrites,
        )?;
        pass.report.replacements += sidecar.secret_replacements;
        pass.report.sidecar_scrubbed = sidecar.secret_replacements > 0;

        let extensions_dir = session_dir.join(EXTENSIONS_DIR);
        for extension_dir in child_dirs(&extensions_dir)? {
            for path in state_files(&extension_dir)? {
                let scrubbed = scrub_json_file(&path, secrets, &pass.reference_rewrites)?;
                if scrubbed.changed {
                    pass.report.extension_state_files_rewritten += 1;
                }
                pass.report.replacements += scrubbed.secret_replacements;
            }
        }
        Ok(())
    }

    fn commit_scrubbed_log(&self, events: &[EventEnvelope]) -> io::Result<()> {
        let file_name = self
            .log_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("events.jsonl");
        let temp_path = self
            .log_path
            .with_file_name(format!(".{file_name}.{}.scrub.tmp", ulid::Ulid::new()));
        let result = (|| {
            let mut file = crate::home::private_open_options()
                .create_new(true)
                .write(true)
                .open(&temp_path)?;
            crate::home::set_file_mode_0600(&file)?;
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
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        result
    }
}

fn scrub_inline_response_chunk(
    event: &mut EventEnvelope,
    secrets: &[String],
    collapsed: bool,
    pass: &mut ScrubPass,
) -> io::Result<bool> {
    if event.kind.as_str() != EventKind::ASSISTANT_RESPONSE_CHUNK
        || collapsed
        || event.blobs.contains_key("content")
    {
        return Ok(false);
    }
    let content = event
        .payload
        .get("content")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_data("response chunk has no content"))?;
    let (scrubbed, replacements) = scrub_secrets_in_bytes(content.as_bytes(), secrets);
    if replacements == 0 {
        return Ok(false);
    }
    let scrubbed = String::from_utf8(scrubbed)
        .map_err(|_| invalid_data("scrubbed response chunk is not valid UTF-8"))?;
    event.payload.insert("content".to_owned(), scrubbed.into());
    pass.report.replacements += replacements;
    Ok(true)
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum StoreKind {
    Blob,
    Checkpoint,
    ExtensionArtifact,
}

#[derive(Clone, Debug)]
struct ContentRewrite {
    new_hash: String,
    scrubbed: Vec<u8>,
}

#[derive(Default)]
struct ScrubPass {
    report: ScrubReport,
    log_changed: bool,
    rewrites: HashMap<(StoreKind, String), Option<ContentRewrite>>,
    examined_paths: BTreeSet<PathBuf>,
    new_files: BTreeMap<PathBuf, Vec<u8>>,
    retired_files: BTreeMap<PathBuf, Vec<u8>>,
    reference_rewrites: Vec<(String, String)>,
    response_old_retained_bytes: HashMap<String, u64>,
    response_new_retained_bytes: HashMap<String, u64>,
    cross_boundary_responses: HashSet<String>,
}

impl ScrubPass {
    fn is_collapsed_response_chunk(&self, event: &EventEnvelope) -> bool {
        event.kind.as_str() == EventKind::ASSISTANT_RESPONSE_CHUNK
            && event
                .payload
                .get("response_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|response_id| self.cross_boundary_responses.contains(response_id))
    }

    fn collapse_cross_boundary_response_chunk(&self, event: &mut EventEnvelope) -> bool {
        if !self.is_collapsed_response_chunk(event) {
            return false;
        }
        let changed = event
            .payload
            .get("content")
            .and_then(serde_json::Value::as_str)
            != Some(SCRUBBED);
        event.payload.insert("content".to_owned(), SCRUBBED.into());
        changed
    }

    /// A cross-chunk secret is absent from each physical blob considered in
    /// isolation. Force every old chunk blob through the ordinary staged
    /// rewrite cache so all references move to a durable marker before the
    /// old hash is sanitized and retired. Shared hashes are deliberately
    /// over-redacted rather than left dangling.
    fn force_blob_scrub(&mut self, cache_key: String, path: PathBuf) -> io::Result<()> {
        if self
            .rewrites
            .contains_key(&(StoreKind::Blob, cache_key.clone()))
        {
            return Ok(());
        }
        self.examined_paths.insert(path.clone());
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(invalid_data(format!(
                "content-addressed scrub surface is not a regular file: {}",
                path.display()
            )));
        }
        let bytes = fs::read(&path)?;
        if hash_bytes(&bytes) != cache_key {
            return Err(invalid_data("response chunk blob hash mismatch"));
        }

        let scrubbed = SCRUBBED.as_bytes().to_vec();
        let new_hash = hash_bytes(&scrubbed);
        let new_path = containing_dir(&path).join(&new_hash);
        self.new_files
            .entry(new_path)
            .or_insert_with(|| scrubbed.clone());
        if cache_key != new_hash {
            self.retired_files
                .entry(path)
                .or_insert_with(|| scrubbed.clone());
            self.report.blobs_rewritten += 1;
        }
        self.rewrites.insert(
            (StoreKind::Blob, cache_key),
            Some(ContentRewrite { new_hash, scrubbed }),
        );
        Ok(())
    }

    /// Secret replacement may change UTF-8 byte length. Rewrite only the
    /// retained-content counter and its canonical terminal; the immutable
    /// locally observed provider byte count remains audit evidence.
    fn reaccount_response_bytes(
        &mut self,
        event: &mut EventEnvelope,
        rewritten_content_bytes: Option<usize>,
    ) -> bool {
        let Some(response_id) = event
            .payload
            .get("response_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
        else {
            return false;
        };
        if event.kind.as_str() == EventKind::ASSISTANT_RESPONSE_CHUNK {
            let Some(old_total) = event
                .payload
                .get("retained_content_bytes")
                .and_then(serde_json::Value::as_u64)
            else {
                return false;
            };
            let old_previous = self
                .response_old_retained_bytes
                .get(&response_id)
                .copied()
                .unwrap_or(0);
            let Some(original_chunk_bytes) = old_total.checked_sub(old_previous) else {
                return false;
            };
            let chunk_bytes = rewritten_content_bytes
                .and_then(|bytes| u64::try_from(bytes).ok())
                .unwrap_or(original_chunk_bytes);
            let new_previous = self
                .response_new_retained_bytes
                .get(&response_id)
                .copied()
                .unwrap_or(0);
            let Some(new_total) = new_previous.checked_add(chunk_bytes) else {
                return false;
            };
            self.response_old_retained_bytes
                .insert(response_id.clone(), old_total);
            self.response_new_retained_bytes
                .insert(response_id, new_total);
            if new_total == old_total {
                return false;
            }
            event
                .payload
                .insert("retained_content_bytes".to_owned(), new_total.into());
            return true;
        }
        if !matches!(
            event.kind.as_str(),
            EventKind::MODEL_RESULT | EventKind::ERROR
        ) {
            return false;
        }
        let Some(new_total) = self.response_new_retained_bytes.get(&response_id).copied() else {
            return false;
        };
        let old_total = event
            .payload
            .get("retained_content_bytes")
            .and_then(serde_json::Value::as_u64);
        if old_total == Some(new_total) {
            return false;
        }
        event
            .payload
            .insert("retained_content_bytes".to_owned(), new_total.into());
        true
    }

    fn rewrite_cached_file(
        &mut self,
        kind: StoreKind,
        cache_key: String,
        path: PathBuf,
        secrets: &[String],
    ) -> io::Result<Option<ContentRewrite>> {
        if let Some(rewrite) = self.rewrites.get(&(kind, cache_key.clone())) {
            return Ok(rewrite.clone());
        }
        let rewrite = self.prepare_file_rewrite(kind, &path, secrets)?;
        self.rewrites.insert((kind, cache_key), rewrite.clone());
        Ok(rewrite)
    }

    fn prepare_file_rewrite(
        &mut self,
        kind: StoreKind,
        path: &Path,
        secrets: &[String],
    ) -> io::Result<Option<ContentRewrite>> {
        self.examined_paths.insert(path.to_path_buf());
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(invalid_data(format!(
                "content-addressed scrub surface is not a regular file: {}",
                path.display()
            )));
        }
        let bytes = fs::read(path)?;
        let (scrubbed, replacements) = scrub_secrets_in_bytes(&bytes, secrets);
        if replacements == 0 {
            return Ok(None);
        }
        let new_hash = hash_bytes(&scrubbed);
        let new_path = containing_dir(path).join(&new_hash);
        self.new_files
            .entry(new_path)
            .or_insert_with(|| scrubbed.clone());
        if path.file_name().and_then(|name| name.to_str()) != Some(new_hash.as_str()) {
            self.retired_files
                .entry(path.to_path_buf())
                .or_insert_with(|| scrubbed.clone());
        }
        self.report.replacements += replacements;
        match kind {
            StoreKind::Blob => self.report.blobs_rewritten += 1,
            StoreKind::Checkpoint => self.report.checkpoints_rewritten += 1,
            StoreKind::ExtensionArtifact => self.report.extension_artifacts_rewritten += 1,
        }
        Ok(Some(ContentRewrite { new_hash, scrubbed }))
    }

    fn sweep_dir(&mut self, kind: StoreKind, dir: &Path, secrets: &[String]) -> io::Result<()> {
        for path in child_files(dir)? {
            if self.examined_paths.contains(&path) {
                continue;
            }
            self.prepare_orphan_retirement(kind, &path, secrets)?;
        }
        Ok(())
    }

    fn prepare_orphan_retirement(
        &mut self,
        kind: StoreKind,
        path: &Path,
        secrets: &[String],
    ) -> io::Result<()> {
        self.examined_paths.insert(path.to_path_buf());
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(invalid_data(format!(
                "content-addressed scrub surface is not a regular file: {}",
                path.display()
            )));
        }
        let bytes = fs::read(path)?;
        let (scrubbed, replacements) = scrub_secrets_in_bytes(&bytes, secrets);
        if replacements == 0 {
            return Ok(());
        }
        self.retired_files.insert(path.to_path_buf(), scrubbed);
        self.report.replacements += replacements;
        match kind {
            StoreKind::Blob => self.report.blobs_rewritten += 1,
            StoreKind::Checkpoint => self.report.checkpoints_rewritten += 1,
            StoreKind::ExtensionArtifact => self.report.extension_artifacts_rewritten += 1,
        }
        Ok(())
    }

    fn persist_new_files(&self) -> io::Result<()> {
        for (path, bytes) in &self.new_files {
            write_content_addressed(path, bytes)?;
        }
        Ok(())
    }

    fn retire_old_files(&self) -> io::Result<()> {
        let mut synced_dirs = BTreeSet::new();
        for (path, scrubbed) in &self.retired_files {
            let metadata = match fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(invalid_data(format!(
                    "retired scrub surface is not a regular file: {}",
                    path.display()
                )));
            }
            // Sanitize first. If deletion is denied, the leftover path no
            // longer contains the credential and the success audit remains
            // truthful.
            write_private_atomic(path, scrubbed)?;
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => {}
            }
            synced_dirs.insert(containing_dir(path).to_path_buf());
        }
        for dir in synced_dirs {
            sync_dir(&dir)?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct ResponseSeamState {
    suffix: Vec<u8>,
    replacements: usize,
    collapse: bool,
}

fn record_collapsed_response(
    redactions: &mut HashMap<String, usize>,
    response_id: &str,
    stream: ResponseSeamState,
) {
    if stream.collapse {
        redactions.insert(response_id.to_owned(), stream.replacements.max(1));
    }
}

fn crossing_needle_count(previous: &[u8], current: &[u8], needles: &[Vec<u8>]) -> usize {
    needles
        .iter()
        .filter(|needle| {
            (1..needle.len()).any(|split| {
                previous.ends_with(&needle[..split]) && current.starts_with(&needle[split..])
            })
        })
        .count()
}

fn retain_bounded_suffix(suffix: &mut Vec<u8>, content: &[u8], bound: usize) {
    if bound == 0 {
        suffix.clear();
        return;
    }
    if content.len() >= bound {
        suffix.clear();
        suffix.extend_from_slice(&content[content.len() - bound..]);
        return;
    }
    let prior_to_keep = bound - content.len();
    if suffix.len() > prior_to_keep {
        suffix.drain(..suffix.len() - prior_to_keep);
    }
    suffix.extend_from_slice(content);
}

fn write_content_addressed(path: &Path, bytes: &[u8]) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(invalid_data(format!(
                    "content-addressed target is not a regular file: {}",
                    path.display()
                )));
            }
            if fs::read(path)? != bytes {
                return Err(invalid_data(format!(
                    "content-addressed target has unexpected bytes: {}",
                    path.display()
                )));
            }
            let file = OpenOptions::new().read(true).open(path)?;
            crate::home::set_file_mode_0600(&file)?;
            file.sync_data()
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => write_private_atomic(path, bytes),
        Err(error) => Err(error),
    }
}

fn required_payload_string(event: &EventEnvelope, field: &str) -> io::Result<String> {
    event
        .payload
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| invalid_data(format!("extension artifact is missing `{field}`")))
}

fn child_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    child_entries(dir, false)
}

fn child_dirs(dir: &Path) -> io::Result<Vec<PathBuf>> {
    child_entries(dir, true)
}

fn child_entries(dir: &Path, directories: bool) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            return Err(invalid_data(format!(
                "scrub surface contains a symlink: {}",
                entry.path().display()
            )));
        }
        if metadata.is_dir() {
            if directories {
                paths.push(entry.path());
            }
            continue;
        }
        if metadata.is_file() {
            if !directories {
                paths.push(entry.path());
            }
            continue;
        }
        return Err(invalid_data(format!(
            "scrub surface contains an unsupported entry: {}",
            entry.path().display()
        )));
    }
    paths.sort();
    Ok(paths)
}

fn state_files(extension_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut pending = vec![extension_dir.to_path_buf()];
    let mut files = Vec::new();
    while let Some(dir) = pending.pop() {
        for child in child_dirs(&dir)? {
            if child.file_name().and_then(|name| name.to_str()) != Some(ARTIFACTS_DIR) {
                pending.push(child);
            }
        }
        files.extend(child_files(&dir)?);
    }
    files.sort();
    Ok(files)
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn scrub_audit_payload(requested_values: usize, report: &ScrubReport) -> euler_event::JsonObject {
    let mut surfaces = serde_json::Map::new();
    surfaces.insert("events".to_owned(), report.events_rewritten.into());
    surfaces.insert("blobs".to_owned(), report.blobs_rewritten.into());
    surfaces.insert(
        "checkpoints".to_owned(),
        report.checkpoints_rewritten.into(),
    );
    surfaces.insert(
        "extension_artifacts".to_owned(),
        report.extension_artifacts_rewritten.into(),
    );
    surfaces.insert(
        "extension_state_files".to_owned(),
        report.extension_state_files_rewritten.into(),
    );
    surfaces.insert("sidecar".to_owned(), report.sidecar_scrubbed.into());
    let mut payload = serde_json::Map::new();
    payload.insert("requested_values".to_owned(), requested_values.into());
    payload.insert("replacements".to_owned(), report.replacements.into());
    payload.insert("surfaces".to_owned(), surfaces.into());
    payload.insert(
        "note".to_owned(),
        "already-exported, copied, terminal-scrollback, or pushed data cannot be recalled".into(),
    );
    payload
}
