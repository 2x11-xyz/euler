use crate::home::{containing_dir, ensure_private_dir, private_open_options, set_file_mode_0600};
use crate::home::{sync_dir, EulerHome, EulerHomeError};
use crate::provenance::accepted_prefix_lines;
use euler_sdk::extension_package::{
    apply_install_package, apply_link_package, decode_link_inventory, encode_link_inventory,
    LinkInventoryError,
};
use euler_sdk::{
    load_extension_package, manifest_sha256_hex, parse_extension_manifest_bytes,
    valid_extension_identifier, ExtensionMaterialization, LinkedExtension, LinkedExtensionStatus,
    LoadedExtensionPackage, EXTENSION_MANIFEST_FILE,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

const STATE_LOG_FILE: &str = "state.jsonl";
const STATE_LOG_LOCK_FILE: &str = "state.jsonl.lock";
const LINK_INVENTORY_FILE: &str = "links.json";
const LINK_INVENTORY_LOCK_FILE: &str = "links.json.lock";
const INSTALLED_DIR: &str = "installed";
const STATE_ENTRY_VERSION: u64 = 1;
const REGISTRY_ROOT_SYMLINK_MESSAGE: &str = "extension registry path must not be a symlink";
const REGISTRY_LEAF_SYMLINK_MESSAGE: &str = "extension registry file path must not be a symlink";
pub const EXTENSION_AUDIT_SCHEMA_VERSION: u64 = 1;
#[derive(Clone, Debug)]
pub struct ExtensionRegistry {
    home: EulerHome,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionEnablement {
    Enabled,
    Disabled,
}
impl ExtensionEnablement {
    pub fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ExtensionAuditReport {
    pub schema_version: u64,
    pub entries: Vec<ExtensionAuditEntry>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ExtensionAuditEntry {
    pub id: String,
    pub source_kind: &'static str,
    pub recorded_status: &'static str,
    pub issue_code: ExtensionAuditIssueCode,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ExtensionAuditIssueCode {
    #[serde(rename = "ok")]
    Ok,
    #[serde(rename = "linked-source-missing")]
    LinkedSourceMissing,
    #[serde(rename = "linked-source-unreadable")]
    LinkedSourceUnreadable,
    #[serde(rename = "linked-manifest-invalid")]
    LinkedManifestInvalid,
    #[serde(rename = "linked-manifest-id-mismatch")]
    LinkedManifestIdMismatch,
    #[serde(rename = "linked-manifest-digest-mismatch")]
    LinkedManifestDigestMismatch,
    #[serde(rename = "installed-path-invalid")]
    InstalledPathInvalid,
    #[serde(rename = "installed-snapshot-missing")]
    InstalledSnapshotMissing,
    #[serde(rename = "installed-snapshot-symlink")]
    InstalledSnapshotSymlink,
    #[serde(rename = "installed-snapshot-unreadable")]
    InstalledSnapshotUnreadable,
    #[serde(rename = "installed-manifest-invalid")]
    InstalledManifestInvalid,
    #[serde(rename = "installed-manifest-id-mismatch")]
    InstalledManifestIdMismatch,
    #[serde(rename = "installed-manifest-digest-mismatch")]
    InstalledManifestDigestMismatch,
}
/// Machine-readable failure envelope for `extension audit`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ExtensionAuditErrorReport {
    pub schema_version: u64,
    pub error: ExtensionAuditError,
}
/// Stable audit failure body. `message` is informational; match on `code`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ExtensionAuditError {
    pub code: ExtensionAuditErrorCode,
    pub message: &'static str,
}
/// Stable machine-readable extension audit failure code.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ExtensionAuditErrorCode {
    #[serde(rename = "registry-home-unavailable")]
    RegistryHomeUnavailable,
    #[serde(rename = "registry-inventory-invalid")]
    RegistryInventoryInvalid,
    #[serde(rename = "registry-record-invalid")]
    RegistryRecordInvalid,
    #[serde(rename = "registry-state-invalid")]
    RegistryStateInvalid,
    #[serde(rename = "registry-unavailable")]
    RegistryUnavailable,
}
impl ExtensionAuditErrorReport {
    pub fn from_registry_error(error: &ExtensionRegistryError) -> Self {
        Self {
            schema_version: EXTENSION_AUDIT_SCHEMA_VERSION,
            error: ExtensionAuditError {
                code: ExtensionAuditErrorCode::from_registry_error(error),
                message: "extension registry audit failed",
            },
        }
    }
}
impl ExtensionAuditErrorCode {
    #[must_use]
    pub fn from_registry_error(error: &ExtensionRegistryError) -> Self {
        match error {
            ExtensionRegistryError::Home(_) => Self::RegistryHomeUnavailable,
            ExtensionRegistryError::LinkInventory(_)
            | ExtensionRegistryError::InvalidExtensionId { .. } => Self::RegistryInventoryInvalid,
            ExtensionRegistryError::UnknownLinkedExtension { .. }
            | ExtensionRegistryError::UnknownInstalledExtension { .. }
            | ExtensionRegistryError::WrongExtensionMode { .. } => Self::RegistryRecordInvalid,
            ExtensionRegistryError::UnsupportedVersion { .. }
            | ExtensionRegistryError::InvalidStateLine(_)
            | ExtensionRegistryError::Serialize(_) => Self::RegistryStateInvalid,
            ExtensionRegistryError::Io(_) => Self::RegistryUnavailable,
        }
    }
}

impl ExtensionRegistry {
    pub fn new(home: EulerHome) -> Result<Self, ExtensionRegistryError> {
        ensure_registry_dir(&home)?;
        Ok(Self { home })
    }
    pub fn open_read_only(home: EulerHome) -> Self {
        Self { home }
    }
    pub fn home(&self) -> &EulerHome {
        &self.home
    }
    pub fn state(&self, id: &str) -> Result<ExtensionEnablement, ExtensionRegistryError> {
        validate_extension_id(id)?;
        let states = self.fold_states()?;
        Ok(states
            .get(id)
            .copied()
            .unwrap_or(ExtensionEnablement::Disabled))
    }
    pub fn enablement_states(
        &self,
    ) -> Result<BTreeMap<String, ExtensionEnablement>, ExtensionRegistryError> {
        self.fold_states()
    }
    pub fn set_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<ExtensionEnablement, ExtensionRegistryError> {
        validate_extension_id(id)?;
        let _lock = self.acquire_lock()?;
        let _states = self.fold_states()?;
        let state = if enabled {
            ExtensionEnablement::Enabled
        } else {
            ExtensionEnablement::Disabled
        };
        self.append_entry_locked(StateEntry::new(id, state))?;
        Ok(state)
    }
    pub fn enable(&self, id: &str) -> Result<ExtensionEnablement, ExtensionRegistryError> {
        self.set_enabled(id, true)
    }
    pub fn disable(&self, id: &str) -> Result<ExtensionEnablement, ExtensionRegistryError> {
        self.set_enabled(id, false)
    }
    pub fn linked_extension(
        &self,
        id: &str,
    ) -> Result<Option<LinkedExtension>, ExtensionRegistryError> {
        validate_extension_id(id)?;
        Ok(self.read_link_inventory()?.remove(id))
    }
    pub fn linked_extensions(&self) -> Result<Vec<LinkedExtension>, ExtensionRegistryError> {
        Ok(self.read_link_inventory()?.into_values().collect())
    }
    pub fn audit(&self) -> Result<ExtensionAuditReport, ExtensionRegistryError> {
        let entries = self
            .read_link_inventory()?
            .values()
            .map(|record| self.audit_record(record))
            .collect();
        Ok(ExtensionAuditReport {
            schema_version: EXTENSION_AUDIT_SCHEMA_VERSION,
            entries,
        })
    }
    pub fn link_package(
        &self,
        package: LoadedExtensionPackage,
    ) -> Result<LinkedExtension, ExtensionRegistryError> {
        validate_extension_id(&package.descriptor.id)?;
        let _lock = self.acquire_link_lock()?;
        let mut links = self.read_link_inventory()?;
        let linked = apply_link_package(&mut links, package)?;
        self.write_link_inventory_locked(&links)?;
        Ok(linked)
    }
    pub fn install_package(
        &self,
        package: LoadedExtensionPackage,
    ) -> Result<LinkedExtension, ExtensionRegistryError> {
        validate_extension_id(&package.descriptor.id)?;
        let manifest_bytes = package.manifest_bytes.clone();
        let _lock = self.acquire_link_lock()?;
        let mut links = self.read_link_inventory()?;
        let existing_same_install = links.get(&package.descriptor.id).is_some_and(|existing| {
            existing.materialization == ExtensionMaterialization::Installed
                && existing.manifest_sha256 == package.manifest_sha256
        });
        let installed_dir =
            self.installed_package_dir(&package.descriptor.id, &package.manifest_sha256);
        let installed = apply_install_package(&mut links, package, installed_dir)?;
        if let Err(error) = self.write_installed_manifest(&installed.source_path, &manifest_bytes) {
            if !existing_same_install {
                let _ = self.remove_installed_snapshot(&installed.source_path);
            }
            return Err(error);
        }
        self.write_link_inventory_locked(&links)?;
        Ok(installed)
    }
    pub fn reload_link(&self, id: &str) -> Result<LinkedExtension, ExtensionRegistryError> {
        validate_extension_id(id)?;
        let _lock = self.acquire_link_lock()?;
        let mut links = self.read_link_inventory()?;
        let existing = extension_record(
            &links,
            id,
            ExtensionMaterialization::Linked,
            MissingExtensionRecord::Linked,
        )?;
        links.remove(id);
        let linked = match load_extension_package(&existing.source_path) {
            Ok(package) if package.descriptor.id == id => {
                LinkedExtension::from_package(package, LinkedExtensionStatus::NeedsReview)
            }
            Ok(package) => {
                let reason = format!("manifest id changed from {id} to {}", package.descriptor.id);
                existing.with_broken(reason)
            }
            Err(error) => existing.with_broken(error.to_string()),
        };
        links.insert(id.to_owned(), linked.clone());
        self.write_link_inventory_locked(&links)?;
        Ok(linked)
    }
    pub fn unlink(&self, id: &str) -> Result<(), ExtensionRegistryError> {
        validate_extension_id(id)?;
        let _lock = self.acquire_link_lock()?;
        let mut links = self.read_link_inventory()?;
        extension_record(
            &links,
            id,
            ExtensionMaterialization::Linked,
            MissingExtensionRecord::Linked,
        )?;
        links.remove(id);
        self.write_link_inventory_locked(&links)?;
        Ok(())
    }
    pub fn uninstall_installed(&self, id: &str) -> Result<LinkedExtension, ExtensionRegistryError> {
        validate_extension_id(id)?;
        let _lock = self.acquire_link_lock()?;
        let mut links = self.read_link_inventory()?;
        let existing = extension_record(
            &links,
            id,
            ExtensionMaterialization::Installed,
            MissingExtensionRecord::Installed,
        )?;
        validate_installed_snapshot_for_removal(&existing.source_path)?;
        links.remove(id);
        self.write_link_inventory_locked(&links)?;
        self.remove_installed_snapshot(&existing.source_path)?;
        Ok(existing)
    }
    fn fold_states(&self) -> Result<BTreeMap<String, ExtensionEnablement>, ExtensionRegistryError> {
        let mut states = BTreeMap::new();
        for entry in self.read_entries()? {
            states.insert(entry.id, entry.op.enablement());
        }
        Ok(states)
    }
    fn read_entries(&self) -> Result<Vec<StateEntry>, ExtensionRegistryError> {
        // Readers intentionally do not take the advisory lock. Writers append
        // one newline-terminated JSON value while holding the lock, and readers
        // fold only the accepted newline-complete prefix.
        let path = self.state_log_path();
        let Some(content) = read_private_string(&path)? else {
            return Ok(Vec::new());
        };
        accepted_prefix_lines(&content)
            .into_iter()
            .map(|line| {
                let entry = serde_json::from_str::<StateEntry>(line)
                    .map_err(ExtensionRegistryError::InvalidStateLine)?;
                validate_entry(&entry)?;
                Ok(entry)
            })
            .collect()
    }

    fn append_entry_locked(&self, entry: StateEntry) -> Result<(), ExtensionRegistryError> {
        let line = serde_json::to_string(&entry).map_err(ExtensionRegistryError::Serialize)?;
        let mut entry_line = line.into_bytes();
        entry_line.push(b'\n');
        let path = self.state_log_path();
        reject_symlink(&path, REGISTRY_LEAF_SYMLINK_MESSAGE)?;
        let mut file = private_no_follow_open_options()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(ExtensionRegistryError::Io)?;
        set_file_mode_0600(&file)?;
        file.write_all(&entry_line)?;
        file.flush()?;
        file.sync_data()?;
        sync_dir(containing_dir(&path))?;
        Ok(())
    }

    fn read_link_inventory(
        &self,
    ) -> Result<BTreeMap<String, LinkedExtension>, ExtensionRegistryError> {
        let path = self.link_inventory_path();
        let Some(content) = read_private_string(&path)? else {
            return Ok(BTreeMap::new());
        };
        Ok(decode_link_inventory(&content)?)
    }

    fn write_link_inventory_locked(
        &self,
        links: &BTreeMap<String, LinkedExtension>,
    ) -> Result<(), ExtensionRegistryError> {
        let path = self.link_inventory_path();
        write_private_file(
            &path,
            &self.link_inventory_tmp_path(),
            &encode_link_inventory(links)?,
        )
    }

    fn write_installed_manifest(
        &self,
        installed_dir: &Path,
        manifest_bytes: &[u8],
    ) -> Result<(), ExtensionRegistryError> {
        ensure_registry_dir(&self.home)?;
        let installed_root = self.installed_extensions_dir();
        ensure_managed_install_path(&installed_root, installed_dir)?;
        for path in [
            installed_root.as_path(),
            containing_dir(installed_dir),
            installed_dir,
        ] {
            ensure_private_non_symlink_dir(path)?;
        }
        let manifest_path = installed_dir.join(EXTENSION_MANIFEST_FILE);
        let tmp = installed_dir.join(format!("{EXTENSION_MANIFEST_FILE}.tmp"));
        write_private_file(&manifest_path, &tmp, manifest_bytes)?;
        sync_dir(installed_dir)?;
        sync_dir(&installed_root)?;
        Ok(())
    }

    fn remove_installed_snapshot(
        &self,
        installed_dir: &Path,
    ) -> Result<(), ExtensionRegistryError> {
        let installed_root = self.installed_extensions_dir();
        ensure_managed_install_path(&installed_root, installed_dir)?;
        validate_installed_snapshot_for_removal(installed_dir)?;
        match fs::remove_dir_all(installed_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(ExtensionRegistryError::Io(source)),
        }
        match fs::remove_dir(containing_dir(installed_dir)) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(source) => return Err(ExtensionRegistryError::Io(source)),
        }
        if installed_root.exists() {
            sync_dir(&installed_root)?;
        }
        Ok(())
    }

    fn acquire_lock(&self) -> Result<File, ExtensionRegistryError> {
        ensure_registry_dir(&self.home)?;
        acquire_file_lock(self.state_log_lock_path())
    }

    fn acquire_link_lock(&self) -> Result<File, ExtensionRegistryError> {
        ensure_registry_dir(&self.home)?;
        acquire_file_lock(self.link_inventory_lock_path())
    }

    fn state_log_path(&self) -> PathBuf {
        self.home.extensions_dir().join(STATE_LOG_FILE)
    }

    fn state_log_lock_path(&self) -> PathBuf {
        self.home.extensions_dir().join(STATE_LOG_LOCK_FILE)
    }

    fn link_inventory_path(&self) -> PathBuf {
        self.home.extensions_dir().join(LINK_INVENTORY_FILE)
    }

    fn link_inventory_tmp_path(&self) -> PathBuf {
        self.home
            .extensions_dir()
            .join(format!("{LINK_INVENTORY_FILE}.tmp"))
    }

    fn link_inventory_lock_path(&self) -> PathBuf {
        self.home.extensions_dir().join(LINK_INVENTORY_LOCK_FILE)
    }

    fn installed_extensions_dir(&self) -> PathBuf {
        self.home.extensions_dir().join(INSTALLED_DIR)
    }

    fn installed_package_dir(&self, id: &str, manifest_sha256: &str) -> PathBuf {
        self.installed_extensions_dir()
            .join(id)
            .join(manifest_sha256)
    }

    fn audit_record(&self, record: &LinkedExtension) -> ExtensionAuditEntry {
        let issue_code = match record.materialization {
            ExtensionMaterialization::Linked => audit_linked_record(record),
            ExtensionMaterialization::Installed => self.audit_installed_record(record),
        };
        ExtensionAuditEntry {
            id: record.id.clone(),
            source_kind: record.materialization.as_str(),
            recorded_status: record.status.as_str(),
            issue_code,
        }
    }

    fn audit_installed_record(&self, record: &LinkedExtension) -> ExtensionAuditIssueCode {
        let installed_root = self.installed_extensions_dir();
        if ensure_managed_install_path(&installed_root, &record.source_path).is_err() {
            return ExtensionAuditIssueCode::InstalledPathInvalid;
        }
        if let Some(issue) = audit_installed_parent_dir(containing_dir(&record.source_path)) {
            return issue;
        }
        if let Some(issue) = audit_installed_snapshot_dir(&record.source_path) {
            return issue;
        }
        let manifest_path = record.source_path.join(EXTENSION_MANIFEST_FILE);
        match fs::symlink_metadata(&manifest_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return ExtensionAuditIssueCode::InstalledSnapshotSymlink;
            }
            Ok(metadata) if !metadata.is_file() => {
                return ExtensionAuditIssueCode::InstalledSnapshotUnreadable;
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return ExtensionAuditIssueCode::InstalledSnapshotMissing;
            }
            Err(_) => return ExtensionAuditIssueCode::InstalledSnapshotUnreadable,
        }
        let bytes = match fs::read(&manifest_path) {
            Ok(bytes) => bytes,
            Err(_) => return ExtensionAuditIssueCode::InstalledSnapshotUnreadable,
        };
        let descriptor = match parse_extension_manifest_bytes(&bytes) {
            Ok(descriptor) => descriptor,
            Err(_) => return ExtensionAuditIssueCode::InstalledManifestInvalid,
        };
        if descriptor.id != record.id {
            return ExtensionAuditIssueCode::InstalledManifestIdMismatch;
        }
        if manifest_sha256_hex(&bytes) != record.manifest_sha256 {
            return ExtensionAuditIssueCode::InstalledManifestDigestMismatch;
        }
        ExtensionAuditIssueCode::Ok
    }
}

fn audit_linked_record(record: &LinkedExtension) -> ExtensionAuditIssueCode {
    match fs::symlink_metadata(&record.source_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return ExtensionAuditIssueCode::LinkedSourceUnreadable;
        }
        Ok(metadata) if !metadata.is_dir() => {
            return ExtensionAuditIssueCode::LinkedSourceUnreadable;
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return ExtensionAuditIssueCode::LinkedSourceMissing;
        }
        Err(_) => return ExtensionAuditIssueCode::LinkedSourceUnreadable,
    }
    let manifest_path = record.source_path.join(EXTENSION_MANIFEST_FILE);
    match fs::symlink_metadata(&manifest_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return ExtensionAuditIssueCode::LinkedSourceUnreadable;
        }
        Ok(metadata) if !metadata.is_file() => {
            return ExtensionAuditIssueCode::LinkedSourceUnreadable;
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return ExtensionAuditIssueCode::LinkedSourceMissing;
        }
        Err(_) => return ExtensionAuditIssueCode::LinkedSourceUnreadable,
    }
    let bytes = match fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return ExtensionAuditIssueCode::LinkedSourceMissing;
        }
        Err(_) => return ExtensionAuditIssueCode::LinkedSourceUnreadable,
    };
    let descriptor = match parse_extension_manifest_bytes(&bytes) {
        Ok(descriptor) => descriptor,
        Err(_) => return ExtensionAuditIssueCode::LinkedManifestInvalid,
    };
    if descriptor.id != record.id {
        return ExtensionAuditIssueCode::LinkedManifestIdMismatch;
    }
    if manifest_sha256_hex(&bytes) != record.manifest_sha256 {
        return ExtensionAuditIssueCode::LinkedManifestDigestMismatch;
    }
    ExtensionAuditIssueCode::Ok
}

fn audit_installed_parent_dir(path: &Path) -> Option<ExtensionAuditIssueCode> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Some(ExtensionAuditIssueCode::InstalledPathInvalid)
        }
        Ok(metadata) if !metadata.is_dir() => Some(ExtensionAuditIssueCode::InstalledPathInvalid),
        Ok(_) => None,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Some(ExtensionAuditIssueCode::InstalledSnapshotMissing)
        }
        Err(_) => Some(ExtensionAuditIssueCode::InstalledPathInvalid),
    }
}

fn audit_installed_snapshot_dir(path: &Path) -> Option<ExtensionAuditIssueCode> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Some(ExtensionAuditIssueCode::InstalledSnapshotSymlink)
        }
        Ok(metadata) if !metadata.is_dir() => {
            Some(ExtensionAuditIssueCode::InstalledSnapshotUnreadable)
        }
        Ok(_) => None,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Some(ExtensionAuditIssueCode::InstalledSnapshotMissing)
        }
        Err(_) => Some(ExtensionAuditIssueCode::InstalledSnapshotUnreadable),
    }
}

fn ensure_registry_dir(home: &EulerHome) -> Result<(), ExtensionRegistryError> {
    let dir = home.extensions_dir();
    reject_symlink(&dir, REGISTRY_ROOT_SYMLINK_MESSAGE)?;
    ensure_private_dir(&dir)?;
    sync_dir(containing_dir(&dir))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum MissingExtensionRecord {
    Linked,
    Installed,
}

impl MissingExtensionRecord {
    fn error(self, id: &str) -> ExtensionRegistryError {
        match self {
            Self::Linked => ExtensionRegistryError::UnknownLinkedExtension { id: id.to_owned() },
            Self::Installed => {
                ExtensionRegistryError::UnknownInstalledExtension { id: id.to_owned() }
            }
        }
    }
}

fn extension_record(
    links: &BTreeMap<String, LinkedExtension>,
    id: &str,
    required: ExtensionMaterialization,
    missing: MissingExtensionRecord,
) -> Result<LinkedExtension, ExtensionRegistryError> {
    let existing = links.get(id).cloned().ok_or_else(|| missing.error(id))?;
    if existing.materialization == required {
        Ok(existing)
    } else {
        Err(wrong_mode(id, existing.materialization, required))
    }
}

fn wrong_mode(
    id: &str,
    existing: ExtensionMaterialization,
    required: ExtensionMaterialization,
) -> ExtensionRegistryError {
    ExtensionRegistryError::WrongExtensionMode {
        id: id.to_owned(),
        existing_mode: existing.as_str(),
        required_mode: required.as_str(),
    }
}

fn write_private_file(path: &Path, tmp: &Path, bytes: &[u8]) -> Result<(), ExtensionRegistryError> {
    reject_symlink(path, REGISTRY_LEAF_SYMLINK_MESSAGE)?;
    prepare_private_tmp(tmp)?;
    let mut file = private_no_follow_open_options()
        .create_new(true)
        .write(true)
        .open(tmp)
        .map_err(ExtensionRegistryError::Io)?;
    set_file_mode_0600(&file)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_data()?;
    reject_symlink(path, REGISTRY_LEAF_SYMLINK_MESSAGE)?;
    fs::rename(tmp, path)?;
    sync_dir(containing_dir(path))?;
    Ok(())
}
fn acquire_file_lock(path: PathBuf) -> Result<File, ExtensionRegistryError> {
    reject_symlink(&path, REGISTRY_LEAF_SYMLINK_MESSAGE)?;
    let lock = private_no_follow_open_options()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
        .map_err(ExtensionRegistryError::Io)?;
    set_file_mode_0600(&lock)?;
    <File as fs4::FileExt>::lock(&lock).map_err(ExtensionRegistryError::Io)?;
    Ok(lock)
}
fn read_private_string(path: &Path) -> Result<Option<String>, ExtensionRegistryError> {
    reject_symlink(containing_dir(path), REGISTRY_ROOT_SYMLINK_MESSAGE)?;
    reject_symlink(path, REGISTRY_LEAF_SYMLINK_MESSAGE)?;
    let mut content = String::new();
    match private_no_follow_open_options().read(true).open(path) {
        Ok(mut file) => file.read_to_string(&mut content).map(|_| Some(content)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(source),
    }
    .map_err(ExtensionRegistryError::Io)
}
fn prepare_private_tmp(path: &Path) -> Result<(), ExtensionRegistryError> {
    reject_symlink(path, REGISTRY_LEAF_SYMLINK_MESSAGE)?;
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(ExtensionRegistryError::Io(source)),
    }
    Ok(())
}
fn private_no_follow_open_options() -> fs::OpenOptions {
    let mut options = private_open_options();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options
}
fn ensure_managed_install_path(root: &Path, path: &Path) -> Result<(), ExtensionRegistryError> {
    let valid = path.strip_prefix(root).ok().is_some_and(|relative| {
        let mut components = relative.components();
        matches!(components.next(), Some(Component::Normal(_)))
            && matches!(components.next(), Some(Component::Normal(_)))
            && components.next().is_none()
    });
    if !valid {
        return Err(ExtensionRegistryError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "installed extension path escaped managed store",
        )));
    }
    Ok(())
}

fn ensure_private_non_symlink_dir(path: &std::path::Path) -> Result<(), ExtensionRegistryError> {
    reject_symlink(path, "extension store path must not be a symlink")?;
    ensure_private_dir(path)?;
    Ok(())
}

// Best-effort preflight, not race-free directory-fd confinement.
fn reject_symlink(path: &Path, message: &str) -> Result<(), ExtensionRegistryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(ExtensionRegistryError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{message}: {}", path.display()),
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(ExtensionRegistryError::Io(source)),
    }
    Ok(())
}

fn validate_installed_snapshot_for_removal(path: &Path) -> Result<(), ExtensionRegistryError> {
    for candidate in [containing_dir(path), path] {
        reject_symlink(candidate, "installed extension path must not be a symlink")?;
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StateEntry {
    v: u64,
    op: StateOp,
    id: String,
    ts_ms: u64,
}

impl StateEntry {
    fn new(id: &str, state: ExtensionEnablement) -> Self {
        Self {
            v: STATE_ENTRY_VERSION,
            op: StateOp::from_enablement(state),
            id: id.to_owned(),
            ts_ms: now_unix_ms(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum StateOp {
    Enable,
    Disable,
}

impl StateOp {
    fn from_enablement(state: ExtensionEnablement) -> Self {
        match state {
            ExtensionEnablement::Enabled => Self::Enable,
            ExtensionEnablement::Disabled => Self::Disable,
        }
    }

    fn enablement(self) -> ExtensionEnablement {
        match self {
            Self::Enable => ExtensionEnablement::Enabled,
            Self::Disable => ExtensionEnablement::Disabled,
        }
    }
}

#[derive(Debug, Error)]
pub enum ExtensionRegistryError {
    #[error(transparent)]
    Home(#[from] EulerHomeError),
    #[error("invalid extension id: {id}")]
    InvalidExtensionId { id: String },
    #[error("registry state log contains unsupported version {version}")]
    UnsupportedVersion { version: u64 },
    #[error("registry state log contains an invalid line: {0}")]
    InvalidStateLine(serde_json::Error),
    #[error("registry link inventory is invalid: {0}")]
    LinkInventory(#[from] LinkInventoryError),
    #[error("unknown linked extension id: {id}")]
    UnknownLinkedExtension { id: String },
    #[error("unknown installed extension id: {id}")]
    UnknownInstalledExtension { id: String },
    #[error("extension id `{id}` is {existing_mode}, not {required_mode}")]
    WrongExtensionMode {
        id: String,
        existing_mode: &'static str,
        required_mode: &'static str,
    },
    #[error("failed to serialize registry state: {0}")]
    Serialize(serde_json::Error),
    #[error("registry io failed: {0}")]
    Io(#[from] io::Error),
}

fn validate_entry(entry: &StateEntry) -> Result<(), ExtensionRegistryError> {
    if entry.v != STATE_ENTRY_VERSION {
        return Err(ExtensionRegistryError::UnsupportedVersion { version: entry.v });
    }
    validate_extension_id(&entry.id)
}

fn validate_extension_id(id: &str) -> Result<(), ExtensionRegistryError> {
    if valid_extension_identifier(id) {
        Ok(())
    } else {
        Err(ExtensionRegistryError::InvalidExtensionId { id: id.to_owned() })
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "extension_registry_test.rs"]
mod extension_registry_test;
