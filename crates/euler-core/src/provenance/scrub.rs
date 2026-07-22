use super::{
    containing_dir, hash_bytes, nul_offset_in_line, numbered_accepted_prefix_lines, recover_mutex,
    sync_dir, ProvenanceWriter,
};
use crate::redaction::{scrub_secrets_in_bytes, scrub_secrets_in_object};
use crate::scrub::{scrub_json_file, write_private_atomic, ScrubReport};
use euler_event::{EventEnvelope, EventKind};
use std::collections::{BTreeMap, BTreeSet, HashMap};
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

        for event in &mut events {
            self.scrub_event(event, secrets, workspace_root, &mut pass)?;
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
        pass: &mut ScrubPass,
    ) -> io::Result<()> {
        let mut changed = false;

        // Artifact routing fields must remain intact until the backing file is
        // resolved and rehashed. The general payload walk follows so user
        // content in every field is still scrubbed.
        if event.kind.as_str() == EventKind::EXTENSION_ARTIFACT {
            changed |= self.scrub_extension_artifact(event, secrets, pass)?;
        }
        let replacements = scrub_secrets_in_object(&mut event.payload, secrets);
        if replacements > 0 {
            pass.report.replacements += replacements;
            changed = true;
        }

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
                .insert(field, format!("blob:{}", rewrite.new_hash).into());
            changed = true;
        }

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
}

impl ScrubPass {
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
