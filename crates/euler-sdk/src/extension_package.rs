use crate::Capability;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const EXTENSION_MANIFEST_FILE: &str = "Euler.extension.json";
pub const MAX_EXTENSION_MANIFEST_BYTES: u64 = 64 * 1024;
pub const LINK_INVENTORY_VERSION: u64 = 1;
const MAX_IDENTIFIER_BYTES: usize = 64;
const MAX_DISPLAY_NAME_BYTES: usize = 128;
const MAX_VERSION_BYTES: usize = 64;
const MAX_SUMMARY_BYTES: usize = 512;
const MAX_COMMANDS: usize = 64;
const MAX_CAPABILITIES: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedExtensionPackage {
    pub canonical_dir: PathBuf,
    pub manifest_bytes: Vec<u8>,
    pub manifest_sha256: String,
    pub descriptor: StaticExtensionDescriptor,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StaticExtensionDescriptor {
    pub id: String,
    pub display_name: String,
    pub version: String,
    pub runtime_kind: String,
    pub capabilities: Vec<String>,
    pub commands: Vec<StaticCommandDescriptor>,
}

impl StaticExtensionDescriptor {
    pub fn command(&self, name: &str) -> Option<&StaticCommandDescriptor> {
        self.commands.iter().find(|command| command.name == name)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StaticCommandDescriptor {
    pub name: String,
    pub display_name: String,
    pub summary: String,
    pub required_capabilities: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkedExtension {
    pub id: String,
    pub materialization: ExtensionMaterialization,
    pub source_path: PathBuf,
    pub manifest_sha256: String,
    pub updated_ts_ms: u64,
    pub status: LinkedExtensionStatus,
    pub descriptor: StaticExtensionDescriptor,
    pub broken_reason: Option<String>,
}

impl LinkedExtension {
    pub fn from_package(package: LoadedExtensionPackage, status: LinkedExtensionStatus) -> Self {
        Self::from_package_with_materialization(
            package,
            ExtensionMaterialization::Linked,
            status,
            None,
        )
    }

    pub fn installed_from_package(package: LoadedExtensionPackage, installed_dir: PathBuf) -> Self {
        Self::from_package_with_materialization(
            package,
            ExtensionMaterialization::Installed,
            LinkedExtensionStatus::InstalledInert,
            Some(installed_dir),
        )
    }

    fn from_package_with_materialization(
        package: LoadedExtensionPackage,
        materialization: ExtensionMaterialization,
        status: LinkedExtensionStatus,
        source_path: Option<PathBuf>,
    ) -> Self {
        Self {
            id: package.descriptor.id.clone(),
            materialization,
            source_path: source_path.unwrap_or(package.canonical_dir),
            manifest_sha256: package.manifest_sha256,
            updated_ts_ms: now_unix_ms(),
            status,
            descriptor: package.descriptor,
            broken_reason: None,
        }
    }

    pub fn with_broken(mut self, reason: String) -> Self {
        self.status = LinkedExtensionStatus::Broken;
        self.broken_reason = Some(reason);
        self.updated_ts_ms = now_unix_ms();
        self
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExtensionMaterialization {
    Linked,
    Installed,
}

impl ExtensionMaterialization {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linked => "linked",
            Self::Installed => "installed",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinkedExtensionStatus {
    NeedsReview,
    Broken,
    InstalledInert,
}

impl LinkedExtensionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NeedsReview => "needs-review",
            Self::Broken => "broken",
            Self::InstalledInert => "installed-inert",
        }
    }
}

#[derive(Debug, Error)]
pub enum ExtensionPackageError {
    #[error("extension path is not valid UTF-8")]
    NonUtf8Path,
    #[error("extension path is not a directory: {path}")]
    NotDirectory { path: String },
    #[error("extension manifest is too large: {bytes} bytes exceeds {limit} bytes")]
    ManifestTooLarge { bytes: u64, limit: u64 },
    #[error("extension manifest io failed at {path}: {source}")]
    Io { path: String, source: io::Error },
    #[error("invalid extension manifest: {0}")]
    InvalidManifest(String),
}

#[derive(Debug, Error)]
pub enum LinkInventoryError {
    #[error("link inventory contains unsupported version {version}")]
    UnsupportedVersion { version: u64 },
    #[error("invalid extension id: {id}")]
    InvalidExtensionId { id: String },
    #[error("extension link path is not valid UTF-8")]
    NonUtf8LinkPath,
    #[error("extension id `{id}` is already linked from {existing_path}, not {requested_path}")]
    LinkIdConflict {
        id: String,
        existing_path: String,
        requested_path: String,
    },
    #[error("extension path {path} is already linked as `{existing_id}`, not `{requested_id}`")]
    LinkPathConflict {
        path: String,
        existing_id: String,
        requested_id: String,
    },
    #[error("extension id `{id}` is already {existing_mode}; remove it before adding it as {requested_mode}")]
    ModeConflict {
        id: String,
        existing_mode: &'static str,
        requested_mode: &'static str,
    },
    #[error("extension id `{id}` is already installed with manifest {existing_manifest_sha256}, not {requested_manifest_sha256}")]
    InstallManifestConflict {
        id: String,
        existing_manifest_sha256: String,
        requested_manifest_sha256: String,
    },
    #[error("link inventory JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn load_extension_package(
    path: &Path,
) -> Result<LoadedExtensionPackage, ExtensionPackageError> {
    let input_path = display_path(path)?;
    let canonical_dir = fs::canonicalize(path).map_err(|source| ExtensionPackageError::Io {
        path: input_path,
        source,
    })?;
    let metadata = fs::metadata(&canonical_dir).map_err(|source| ExtensionPackageError::Io {
        path: display_path(&canonical_dir).unwrap_or_else(|_| "<non-utf8>".to_owned()),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(ExtensionPackageError::NotDirectory {
            path: display_path(&canonical_dir)?,
        });
    }

    let manifest_path = canonical_dir.join(EXTENSION_MANIFEST_FILE);
    let metadata = fs::metadata(&manifest_path).map_err(|source| ExtensionPackageError::Io {
        path: display_path(&manifest_path).unwrap_or_else(|_| "<non-utf8>".to_owned()),
        source,
    })?;
    if !metadata.is_file() {
        return Err(ExtensionPackageError::InvalidManifest(format!(
            "{EXTENSION_MANIFEST_FILE} is not a regular file"
        )));
    }
    if metadata.len() > MAX_EXTENSION_MANIFEST_BYTES {
        return Err(ExtensionPackageError::ManifestTooLarge {
            bytes: metadata.len(),
            limit: MAX_EXTENSION_MANIFEST_BYTES,
        });
    }

    let bytes = fs::read(&manifest_path).map_err(|source| ExtensionPackageError::Io {
        path: display_path(&manifest_path).unwrap_or_else(|_| "<non-utf8>".to_owned()),
        source,
    })?;
    let descriptor = parse_extension_manifest_bytes(&bytes)?;
    Ok(LoadedExtensionPackage {
        canonical_dir,
        manifest_bytes: bytes.clone(),
        manifest_sha256: manifest_sha256_hex(&bytes),
        descriptor,
    })
}

pub fn parse_extension_manifest_bytes(
    bytes: &[u8],
) -> Result<StaticExtensionDescriptor, ExtensionPackageError> {
    if bytes.len() as u64 > MAX_EXTENSION_MANIFEST_BYTES {
        return Err(ExtensionPackageError::ManifestTooLarge {
            bytes: bytes.len() as u64,
            limit: MAX_EXTENSION_MANIFEST_BYTES,
        });
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        ExtensionPackageError::InvalidManifest(format!("manifest is not valid JSON: {error}"))
    })?;
    let root = value.as_object().ok_or_else(|| {
        ExtensionPackageError::InvalidManifest("manifest root must be an object".to_owned())
    })?;
    validate_fields(
        root,
        &[
            "version",
            "id",
            "display_name",
            "extension_version",
            "runtime_kind",
            "capabilities",
            "commands",
        ],
        "manifest",
    )?;

    let version = root
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("manifest version must be integer 1"))?;
    if version != 1 {
        return Err(invalid("manifest version must be integer 1"));
    }
    let id = required_identifier(root, "id", "manifest id")?;
    let display_name = required_bounded_string(
        root,
        "display_name",
        "manifest display_name",
        MAX_DISPLAY_NAME_BYTES,
    )?;
    let extension_version = required_bounded_string(
        root,
        "extension_version",
        "manifest extension_version",
        MAX_VERSION_BYTES,
    )?;
    let runtime_kind = required_bounded_string(
        root,
        "runtime_kind",
        "manifest runtime_kind",
        MAX_VERSION_BYTES,
    )?;
    if runtime_kind != "native-rust" {
        return Err(invalid("manifest runtime_kind must be native-rust"));
    }

    let capabilities = required_string_list(root, "capabilities", "manifest capabilities")?;
    validate_capabilities(&capabilities, "manifest capabilities")?;
    let commands = parse_commands(root)?;
    let envelope = capabilities.iter().cloned().collect::<BTreeSet<_>>();
    for command in &commands {
        for capability in &command.required_capabilities {
            if !envelope.contains(capability) {
                return Err(invalid(format!(
                    "command `{}` requires capability outside manifest envelope: {}",
                    command.name, capability
                )));
            }
        }
    }

    Ok(StaticExtensionDescriptor {
        id,
        display_name,
        version: extension_version,
        runtime_kind,
        capabilities,
        commands,
    })
}

pub fn apply_link_package(
    links: &mut BTreeMap<String, LinkedExtension>,
    package: LoadedExtensionPackage,
) -> Result<LinkedExtension, LinkInventoryError> {
    validate_link_id(&package.descriptor.id)?;
    let source_path = utf8_path(&package.canonical_dir)?;
    if let Some(existing) = links.get(&package.descriptor.id) {
        if existing.materialization == ExtensionMaterialization::Installed {
            return Err(LinkInventoryError::ModeConflict {
                id: package.descriptor.id,
                existing_mode: existing.materialization.as_str(),
                requested_mode: ExtensionMaterialization::Linked.as_str(),
            });
        }
        let existing_path = utf8_path(&existing.source_path)?;
        if existing_path != source_path {
            return Err(LinkInventoryError::LinkIdConflict {
                id: package.descriptor.id,
                existing_path,
                requested_path: source_path,
            });
        }
    }
    for existing in links.values() {
        let existing_path = utf8_path(&existing.source_path)?;
        if existing_path == source_path && existing.id != package.descriptor.id {
            return Err(LinkInventoryError::LinkPathConflict {
                path: source_path,
                existing_id: existing.id.clone(),
                requested_id: package.descriptor.id,
            });
        }
    }
    let linked = LinkedExtension::from_package(package, LinkedExtensionStatus::NeedsReview);
    links.insert(linked.id.clone(), linked.clone());
    Ok(linked)
}

pub fn apply_install_package(
    links: &mut BTreeMap<String, LinkedExtension>,
    package: LoadedExtensionPackage,
    installed_dir: PathBuf,
) -> Result<LinkedExtension, LinkInventoryError> {
    validate_link_id(&package.descriptor.id)?;
    if let Some(existing) = links.get(&package.descriptor.id) {
        if existing.materialization == ExtensionMaterialization::Linked {
            return Err(LinkInventoryError::ModeConflict {
                id: package.descriptor.id,
                existing_mode: existing.materialization.as_str(),
                requested_mode: ExtensionMaterialization::Installed.as_str(),
            });
        }
        if existing.manifest_sha256 == package.manifest_sha256 {
            return Ok(existing.clone());
        }
        return Err(LinkInventoryError::InstallManifestConflict {
            id: package.descriptor.id,
            existing_manifest_sha256: existing.manifest_sha256.clone(),
            requested_manifest_sha256: package.manifest_sha256,
        });
    }
    let installed = LinkedExtension::installed_from_package(package, installed_dir);
    links.insert(installed.id.clone(), installed.clone());
    Ok(installed)
}

pub fn decode_link_inventory(
    content: &str,
) -> Result<BTreeMap<String, LinkedExtension>, LinkInventoryError> {
    let inventory = serde_json::from_str::<LinkInventory>(content)?;
    inventory.into_linked()
}

pub fn encode_link_inventory(
    links: &BTreeMap<String, LinkedExtension>,
) -> Result<Vec<u8>, LinkInventoryError> {
    let inventory = LinkInventory::from_linked(links)?;
    let mut bytes = serde_json::to_vec_pretty(&inventory)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn parse_commands(
    root: &Map<String, Value>,
) -> Result<Vec<StaticCommandDescriptor>, ExtensionPackageError> {
    let commands = root
        .get("commands")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("manifest commands must be an array"))?;
    if commands.len() > MAX_COMMANDS {
        return Err(invalid(format!(
            "manifest commands has {} entries; maximum is {MAX_COMMANDS}",
            commands.len()
        )));
    }
    if commands.is_empty() {
        return Err(invalid("manifest commands must not be empty"));
    }

    let mut seen = BTreeSet::new();
    let mut parsed = Vec::with_capacity(commands.len());
    for (index, command) in commands.iter().enumerate() {
        let scope = format!("manifest command #{index}");
        let object = command
            .as_object()
            .ok_or_else(|| invalid(format!("{scope} must be an object")))?;
        validate_fields(
            object,
            &["name", "display_name", "summary", "required_capabilities"],
            &scope,
        )?;
        let name = required_identifier(object, "name", &format!("{scope} name"))?;
        if !seen.insert(name.clone()) {
            return Err(invalid(format!("duplicate command name: {name}")));
        }
        let display_name = required_bounded_string(
            object,
            "display_name",
            &format!("{scope} display_name"),
            MAX_DISPLAY_NAME_BYTES,
        )?;
        let summary = required_bounded_string(
            object,
            "summary",
            &format!("{scope} summary"),
            MAX_SUMMARY_BYTES,
        )?;
        let required_capabilities = required_string_list(
            object,
            "required_capabilities",
            &format!("{scope} required_capabilities"),
        )?;
        validate_capabilities(
            &required_capabilities,
            &format!("{scope} required_capabilities"),
        )?;
        parsed.push(StaticCommandDescriptor {
            name,
            display_name,
            summary,
            required_capabilities,
        });
    }
    Ok(parsed)
}

fn validate_fields(
    object: &Map<String, Value>,
    allowed: &[&str],
    scope: &str,
) -> Result<(), ExtensionPackageError> {
    for field in object.keys() {
        if allowed.contains(&field.as_str()) {
            continue;
        }
        if forbidden_manifest_field(field) {
            return Err(invalid(format!(
                "forbidden secret-like field `{scope}.{field}`"
            )));
        }
        return Err(invalid(format!("unknown field `{scope}.{field}`")));
    }
    Ok(())
}

fn required_identifier(
    object: &Map<String, Value>,
    field: &str,
    scope: &str,
) -> Result<String, ExtensionPackageError> {
    let value = required_bounded_string(object, field, scope, MAX_IDENTIFIER_BYTES)?;
    if valid_extension_identifier(&value) {
        Ok(value)
    } else {
        Err(invalid(format!(
            "{scope} is not a valid extension identifier"
        )))
    }
}

fn required_bounded_string(
    object: &Map<String, Value>,
    field: &str,
    scope: &str,
    max_bytes: usize,
) -> Result<String, ExtensionPackageError> {
    let value = object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{scope} must be a string")))?;
    if value.trim().is_empty() {
        return Err(invalid(format!("{scope} must not be empty")));
    }
    if value.len() > max_bytes {
        return Err(invalid(format!(
            "{scope} is too long: {} bytes exceeds {max_bytes}",
            value.len()
        )));
    }
    Ok(value.to_owned())
}

fn required_string_list(
    object: &Map<String, Value>,
    field: &str,
    scope: &str,
) -> Result<Vec<String>, ExtensionPackageError> {
    let values = object
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(format!("{scope} must be an array")))?;
    if values.len() > MAX_CAPABILITIES {
        return Err(invalid(format!(
            "{scope} has {} entries; maximum is {MAX_CAPABILITIES}",
            values.len()
        )));
    }
    let mut out = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let Some(value) = value.as_str() else {
            return Err(invalid(format!("{scope} entry #{index} must be a string")));
        };
        if value.trim().is_empty() {
            return Err(invalid(format!("{scope} entry #{index} must not be empty")));
        }
        out.push(value.to_owned());
    }
    Ok(out)
}

fn validate_capabilities(
    capabilities: &[String],
    scope: &str,
) -> Result<(), ExtensionPackageError> {
    let mut seen = BTreeSet::new();
    for capability in capabilities {
        if !valid_capability(capability) {
            return Err(invalid(format!(
                "unknown capability in {scope}: {capability}"
            )));
        }
        if !seen.insert(capability) {
            return Err(invalid(format!(
                "duplicate capability in {scope}: {capability}"
            )));
        }
    }
    Ok(())
}

fn valid_capability(value: &str) -> bool {
    Capability::parse(value).is_some()
}

fn forbidden_manifest_field(field: &str) -> bool {
    let field = field.to_ascii_lowercase();
    [
        "key",
        "secret",
        "token",
        "password",
        "auth",
        "credential",
        "header",
        "base_url",
        "baseurl",
    ]
    .iter()
    .any(|needle| field.contains(needle))
}

fn invalid(message: impl Into<String>) -> ExtensionPackageError {
    ExtensionPackageError::InvalidManifest(message.into())
}

pub fn valid_extension_identifier(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES {
        return false;
    }
    let bytes = value.as_bytes();
    let first_ok = bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit();
    let last_ok =
        bytes[value.len() - 1].is_ascii_lowercase() || bytes[value.len() - 1].is_ascii_digit();
    first_ok
        && last_ok
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn display_path(path: &Path) -> Result<String, ExtensionPackageError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or(ExtensionPackageError::NonUtf8Path)
}

fn utf8_path(path: &Path) -> Result<String, LinkInventoryError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or(LinkInventoryError::NonUtf8LinkPath)
}

fn validate_link_id(id: &str) -> Result<(), LinkInventoryError> {
    if valid_extension_identifier(id) {
        Ok(())
    } else {
        Err(LinkInventoryError::InvalidExtensionId { id: id.to_owned() })
    }
}

pub fn manifest_sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct LinkInventory {
    v: u64,
    links: BTreeMap<String, LinkRecord>,
}

impl LinkInventory {
    fn from_linked(links: &BTreeMap<String, LinkedExtension>) -> Result<Self, LinkInventoryError> {
        let mut records = BTreeMap::new();
        let mut paths = BTreeMap::new();
        for (id, linked) in links {
            validate_link_id(id)?;
            validate_link_id(&linked.id)?;
            if linked.id != *id || linked.descriptor.id != *id {
                return Err(LinkInventoryError::InvalidExtensionId {
                    id: linked.descriptor.id.clone(),
                });
            }
            let path = utf8_path(&linked.source_path)?;
            if let Some(existing_id) = paths.insert(path.clone(), id.clone()) {
                return Err(LinkInventoryError::LinkPathConflict {
                    path,
                    existing_id,
                    requested_id: id.clone(),
                });
            }
            records.insert(id.clone(), LinkRecord::from_linked(linked)?);
        }
        Ok(Self {
            v: LINK_INVENTORY_VERSION,
            links: records,
        })
    }

    fn into_linked(self) -> Result<BTreeMap<String, LinkedExtension>, LinkInventoryError> {
        if self.v != LINK_INVENTORY_VERSION {
            return Err(LinkInventoryError::UnsupportedVersion { version: self.v });
        }
        let mut links = BTreeMap::new();
        let mut paths = BTreeMap::new();
        for (id, record) in self.links {
            validate_link_id(&id)?;
            let linked = record.into_linked(id.clone())?;
            let path = utf8_path(&linked.source_path)?;
            if let Some(existing_id) = paths.insert(path.clone(), id.clone()) {
                return Err(LinkInventoryError::LinkPathConflict {
                    path,
                    existing_id,
                    requested_id: id,
                });
            }
            links.insert(id, linked);
        }
        Ok(links)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct LinkRecord {
    #[serde(default = "default_linked_materialization")]
    materialization: ExtensionMaterialization,
    source_path: String,
    manifest_sha256: String,
    status: LinkedExtensionStatus,
    descriptor: StaticExtensionDescriptor,
    broken_reason: Option<String>,
    updated_ts_ms: u64,
}

impl LinkRecord {
    fn from_linked(linked: &LinkedExtension) -> Result<Self, LinkInventoryError> {
        Ok(Self {
            materialization: linked.materialization,
            source_path: utf8_path(&linked.source_path)?,
            manifest_sha256: linked.manifest_sha256.clone(),
            status: linked.status,
            descriptor: linked.descriptor.clone(),
            broken_reason: linked.broken_reason.clone(),
            updated_ts_ms: linked.updated_ts_ms,
        })
    }

    fn into_linked(self, id: String) -> Result<LinkedExtension, LinkInventoryError> {
        validate_link_id(&self.descriptor.id)?;
        if self.descriptor.id != id {
            return Err(LinkInventoryError::InvalidExtensionId {
                id: self.descriptor.id,
            });
        }
        Ok(LinkedExtension {
            id,
            materialization: self.materialization,
            source_path: PathBuf::from(self.source_path),
            manifest_sha256: self.manifest_sha256,
            updated_ts_ms: self.updated_ts_ms,
            status: self.status,
            descriptor: self.descriptor,
            broken_reason: self.broken_reason,
        })
    }
}

fn default_linked_materialization() -> ExtensionMaterialization {
    ExtensionMaterialization::Linked
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
#[path = "extension_package_test.rs"]
mod extension_package_test;
