use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use euler_provider::catalog::{MergedModelCatalog, EMBEDDED_CATALOG_JSON, EMBEDDED_MANIFEST_JSON};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};
use url::Url;

const LATEST_MANIFEST_URL: &str =
    "https://github.com/2x11-xyz/euler-provider-catalog/releases/latest/download/manifest-v1.json";
const RELEASE_DOWNLOAD_ROOT: &str =
    "https://github.com/2x11-xyz/euler-provider-catalog/releases/download/";
const MANIFEST_LIMIT_BYTES: u64 = 64 * 1024;
const ARTIFACT_LIMIT_BYTES: u64 = 16 * 1024 * 1024;
const CACHE_BUNDLE_LIMIT_BYTES: u64 = 20 * 1024 * 1024;
const CACHE_SCHEMA_VERSION: u64 = 1;
const REFRESH_STATE_FILE: &str = ".refresh-state-v1.json";
const SUCCESS_REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const FAILED_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);
const MAX_RELEASE_CLOCK_SKEW: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_REDIRECTS: usize = 5;
const HTTP_DEADLINE: Duration = Duration::from_secs(30);
const MAX_CACHE_FILES: usize = 128;
const RETAINED_CACHE_RELEASES: usize = 3;
const STALE_TEMP_FILE_AGE: Duration = Duration::from_secs(48 * 60 * 60);
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ArtifactMetadata {
    bytes: u64,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseArtifacts {
    #[serde(rename = "catalog-v1.json")]
    catalog: ArtifactMetadata,
    #[serde(rename = "provenance-v1.json")]
    provenance: ArtifactMetadata,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseManifest {
    schema_version: u64,
    release_id: String,
    generated_at: String,
    minimum_euler_version: String,
    artifacts: ReleaseArtifacts,
}

#[derive(Serialize)]
struct ManifestIdentity<'a> {
    artifacts: &'a ReleaseArtifacts,
    generated_at: &'a str,
    minimum_euler_version: &'a str,
    schema_version: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CachedRelease {
    schema_version: u64,
    manifest: ReleaseManifest,
    catalog_json: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RefreshStateOutcome {
    Succeeded,
    Failed,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RefreshState {
    schema_version: u64,
    attempted_at: String,
    outcome: RefreshStateOutcome,
    release_id: Option<String>,
}

#[derive(Clone)]
struct ValidatedRelease {
    manifest: ReleaseManifest,
    generated_at: DateTime<Utc>,
    catalog: MergedModelCatalog,
    catalog_json: String,
}

pub(crate) struct ManagedCatalogLoad {
    pub(crate) catalog: MergedModelCatalog,
    pub(crate) release_id: String,
    pub(crate) from_cache: bool,
    pub(crate) warnings: Vec<String>,
}

#[derive(Debug)]
pub(crate) enum RefreshOutcome {
    Current { release_id: String },
    Updated { release_id: String },
}

impl RefreshOutcome {
    pub(crate) fn release_id(&self) -> &str {
        match self {
            Self::Current { release_id } | Self::Updated { release_id } => release_id,
        }
    }

    pub(crate) fn was_updated(&self) -> bool {
        matches!(self, Self::Updated { .. })
    }
}

#[derive(Debug)]
pub(crate) struct RefreshReport {
    pub(crate) outcome: RefreshOutcome,
    pub(crate) warnings: Vec<String>,
}

pub(crate) fn managed_catalog_dir_for_model_path(model_path: &Path) -> PathBuf {
    model_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("catalogs")
        .join("provider-v1")
}

pub(crate) fn load_managed_catalog(cache_dir: Option<&Path>) -> ManagedCatalogLoad {
    let embedded = embedded_release();
    let Some(cache_dir) = cache_dir else {
        return managed_load(embedded, false, Vec::new());
    };
    let (cached, warnings) = load_best_cached_release(cache_dir, &embedded);
    match cached {
        Some(release) => managed_load(release, true, warnings),
        None => managed_load(embedded, false, warnings),
    }
}

fn managed_load(
    release: ValidatedRelease,
    from_cache: bool,
    warnings: Vec<String>,
) -> ManagedCatalogLoad {
    ManagedCatalogLoad {
        catalog: release.catalog,
        release_id: release.manifest.release_id,
        from_cache,
        warnings,
    }
}

pub(crate) fn refresh_managed_catalog(cache_dir: &Path) -> Result<RefreshReport> {
    refresh_managed_catalog_with(cache_dir, Utc::now(), fetch_github_asset)
}

fn refresh_managed_catalog_with<F>(
    cache_dir: &Path,
    now: DateTime<Utc>,
    mut fetch: F,
) -> Result<RefreshReport>
where
    F: FnMut(&str, u64) -> Result<Vec<u8>>,
{
    let mut result = refresh_inner(cache_dir, now, &mut fetch);
    if let Ok(report) = &mut result {
        let release_id = report.outcome.release_id().to_owned();
        report
            .warnings
            .extend(maintain_cache(cache_dir, &release_id));
    }
    let (outcome, release_id) = match &result {
        Ok(report) => (
            RefreshStateOutcome::Succeeded,
            Some(report.outcome.release_id().to_owned()),
        ),
        Err(_) => (RefreshStateOutcome::Failed, None),
    };
    let state_result = write_refresh_state(cache_dir, now, outcome, release_id);
    match (result, state_result) {
        (Ok(mut report), Err(error)) => {
            report
                .warnings
                .push(format!("could not record catalog refresh time: {error}"));
            Ok(report)
        }
        (result, _) => result,
    }
}

fn refresh_inner<F>(cache_dir: &Path, now: DateTime<Utc>, fetch: &mut F) -> Result<RefreshReport>
where
    F: FnMut(&str, u64) -> Result<Vec<u8>>,
{
    let embedded = embedded_release();
    let (cached, mut warnings) = load_best_cached_release(cache_dir, &embedded);
    let current = cached.as_ref().unwrap_or(&embedded);
    let manifest_bytes = fetch(LATEST_MANIFEST_URL, MANIFEST_LIMIT_BYTES)
        .context("failed to fetch the latest provider catalog manifest")?;
    let manifest = parse_and_validate_manifest(&manifest_bytes)?;
    ensure_compatible(&manifest)?;
    let candidate_time = parse_generated_at(&manifest.generated_at)?;
    ensure_release_time_is_plausible(candidate_time, now)?;
    match compare_releases(&manifest.release_id, candidate_time, current)? {
        ReleaseOrder::Same => {
            return Ok(RefreshReport {
                outcome: RefreshOutcome::Current {
                    release_id: current.manifest.release_id.clone(),
                },
                warnings,
            });
        }
        ReleaseOrder::Older => {
            return Err(anyhow!(
                "refused provider catalog downgrade from {} to {}",
                current.manifest.release_id,
                manifest.release_id
            ));
        }
        ReleaseOrder::Newer => {}
    }
    let catalog_url = release_catalog_url(&manifest.release_id);
    let catalog_bytes = fetch(&catalog_url, manifest.artifacts.catalog.bytes)?;
    let candidate = validate_release(manifest, catalog_bytes)?;
    let (latest_cached, concurrent_warnings) = load_best_cached_release(cache_dir, &embedded);
    warnings.extend(concurrent_warnings);
    let latest = latest_cached.as_ref().unwrap_or(&embedded);
    match compare_releases(
        &candidate.manifest.release_id,
        candidate.generated_at,
        latest,
    )? {
        ReleaseOrder::Same | ReleaseOrder::Older => {
            return Ok(RefreshReport {
                outcome: RefreshOutcome::Current {
                    release_id: latest.manifest.release_id.clone(),
                },
                warnings,
            });
        }
        ReleaseOrder::Newer => {}
    }
    write_cached_release(cache_dir, &candidate)?;
    Ok(RefreshReport {
        outcome: RefreshOutcome::Updated {
            release_id: candidate.manifest.release_id,
        },
        warnings,
    })
}

fn ensure_release_time_is_plausible(candidate: DateTime<Utc>, now: DateTime<Utc>) -> Result<()> {
    let maximum = now
        + chrono::Duration::from_std(MAX_RELEASE_CLOCK_SKEW)
            .expect("release clock-skew bound must fit chrono");
    if candidate > maximum {
        return Err(anyhow!(
            "provider catalog release time is implausibly far in the future"
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReleaseOrder {
    Older,
    Same,
    Newer,
}

fn compare_releases(
    candidate_id: &str,
    candidate_time: DateTime<Utc>,
    current: &ValidatedRelease,
) -> Result<ReleaseOrder> {
    use std::cmp::Ordering;
    match candidate_time.cmp(&current.generated_at) {
        Ordering::Less => Ok(ReleaseOrder::Older),
        Ordering::Greater => Ok(ReleaseOrder::Newer),
        Ordering::Equal if candidate_id == current.manifest.release_id => Ok(ReleaseOrder::Same),
        Ordering::Equal => Err(anyhow!(
            "provider catalog releases share a timestamp but have different identities"
        )),
    }
}

pub(crate) fn automatic_refresh_due(cache_dir: &Path) -> bool {
    automatic_refresh_due_at(cache_dir, Utc::now())
}

fn automatic_refresh_due_at(cache_dir: &Path, now: DateTime<Utc>) -> bool {
    let Ok(bytes) = read_bounded_file(&cache_dir.join(REFRESH_STATE_FILE), MANIFEST_LIMIT_BYTES)
    else {
        return true;
    };
    let Ok(state) = serde_json::from_slice::<RefreshState>(&bytes) else {
        return true;
    };
    if state.schema_version != CACHE_SCHEMA_VERSION {
        return true;
    }
    let Ok(attempted_at) = parse_generated_at(&state.attempted_at) else {
        return true;
    };
    let elapsed = now.signed_duration_since(attempted_at);
    if elapsed.num_seconds() < 0 {
        return true;
    }
    let interval = match state.outcome {
        RefreshStateOutcome::Succeeded => SUCCESS_REFRESH_INTERVAL,
        RefreshStateOutcome::Failed => FAILED_REFRESH_INTERVAL,
    };
    elapsed.to_std().map_or(true, |elapsed| elapsed >= interval)
}

fn embedded_release() -> ValidatedRelease {
    let manifest = parse_and_validate_manifest(EMBEDDED_MANIFEST_JSON.as_bytes())
        .expect("packaged provider catalog manifest must be valid");
    ensure_compatible(&manifest).expect("packaged provider catalog must support this Euler");
    validate_release(manifest, EMBEDDED_CATALOG_JSON.as_bytes().to_vec())
        .expect("packaged provider catalog release must be valid")
}

fn parse_and_validate_manifest(bytes: &[u8]) -> Result<ReleaseManifest> {
    if bytes.is_empty() || bytes.len() as u64 > MANIFEST_LIMIT_BYTES {
        return Err(anyhow!("provider catalog manifest size is out of bounds"));
    }
    let manifest: ReleaseManifest =
        serde_json::from_slice(bytes).context("provider catalog manifest is not strict JSON")?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn validate_manifest(manifest: &ReleaseManifest) -> Result<()> {
    if manifest.schema_version != 1 {
        return Err(anyhow!(
            "unsupported provider catalog manifest schema {}",
            manifest.schema_version
        ));
    }
    let generated_at = parse_generated_at(&manifest.generated_at)?;
    parse_version(&manifest.minimum_euler_version)
        .context("invalid minimum Euler version in provider catalog manifest")?;
    validate_artifact_metadata(&manifest.artifacts.catalog, "catalog-v1.json")?;
    validate_artifact_metadata(&manifest.artifacts.provenance, "provenance-v1.json")?;
    let expected = release_id(manifest, generated_at)?;
    if manifest.release_id != expected {
        return Err(anyhow!(
            "provider catalog release id does not authenticate its manifest"
        ));
    }
    Ok(())
}

fn validate_artifact_metadata(metadata: &ArtifactMetadata, name: &str) -> Result<()> {
    if metadata.bytes == 0 || metadata.bytes > ARTIFACT_LIMIT_BYTES {
        return Err(anyhow!("provider catalog {name} size is out of bounds"));
    }
    if !is_lower_hex_digest(&metadata.sha256) {
        return Err(anyhow!("provider catalog {name} digest is invalid"));
    }
    Ok(())
}

fn release_id(manifest: &ReleaseManifest, generated_at: DateTime<Utc>) -> Result<String> {
    // Wire-format invariant shared with
    // euler-provider-catalog/catalog_pipeline/common.py::catalog_release_id:
    // identity keys (including nested artifact keys) are lexicographically
    // ordered, JSON uses two-space pretty indentation, and one trailing LF is
    // hashed. Changing this encoding is a catalog protocol change and must be
    // coordinated across both repositories; the embedded release test pins it.
    let identity = ManifestIdentity {
        artifacts: &manifest.artifacts,
        generated_at: &manifest.generated_at,
        minimum_euler_version: &manifest.minimum_euler_version,
        schema_version: 1,
    };
    let mut encoded = serde_json::to_vec_pretty(&identity)?;
    encoded.push(b'\n');
    let timestamp = generated_at.format("%Y%m%dt%H%M%Sz");
    Ok(format!("catalog-v1-{timestamp}-{}", sha256_hex(&encoded)))
}

fn validate_release(manifest: ReleaseManifest, catalog_bytes: Vec<u8>) -> Result<ValidatedRelease> {
    ensure_compatible(&manifest)?;
    let metadata = &manifest.artifacts.catalog;
    if catalog_bytes.len() as u64 != metadata.bytes || sha256_hex(&catalog_bytes) != metadata.sha256
    {
        return Err(anyhow!(
            "provider catalog bytes do not match the release manifest"
        ));
    }
    let catalog_json =
        String::from_utf8(catalog_bytes).context("provider catalog artifact is not valid UTF-8")?;
    let catalog = MergedModelCatalog::from_official_json(&catalog_json)
        .context("provider catalog artifact failed strict validation")?
        .with_official_release_id(manifest.release_id.clone());
    let generated_at = parse_generated_at(&manifest.generated_at)?;
    Ok(ValidatedRelease {
        manifest,
        generated_at,
        catalog,
        catalog_json,
    })
}

fn ensure_compatible(manifest: &ReleaseManifest) -> Result<()> {
    let minimum = parse_version(&manifest.minimum_euler_version)?;
    let current = parse_version(env!("CARGO_PKG_VERSION"))?;
    if current < minimum {
        return Err(anyhow!(
            "provider catalog {} requires Euler {}, but this binary is {}",
            manifest.release_id,
            manifest.minimum_euler_version,
            env!("CARGO_PKG_VERSION")
        ));
    }
    Ok(())
}

fn parse_version(value: &str) -> Result<(u64, u64, u64)> {
    let mut parts = value.split('.');
    let major = parse_version_part(parts.next(), value)?;
    let minor = parse_version_part(parts.next(), value)?;
    let patch = parse_version_part(parts.next(), value)?;
    if parts.next().is_some() {
        return Err(anyhow!("invalid semantic version `{value}`"));
    }
    Ok((major, minor, patch))
}

fn parse_version_part(part: Option<&str>, whole: &str) -> Result<u64> {
    let part = part.ok_or_else(|| anyhow!("invalid semantic version `{whole}`"))?;
    if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(anyhow!("invalid semantic version `{whole}`"));
    }
    part.parse()
        .map_err(|_| anyhow!("invalid semantic version `{whole}`"))
}

fn parse_generated_at(value: &str) -> Result<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("invalid provider catalog timestamp `{value}`"))?
        .with_timezone(&Utc);
    let canonical = parsed.to_rfc3339_opts(SecondsFormat::Secs, true);
    if canonical != value {
        return Err(anyhow!(
            "provider catalog timestamp is not canonical UTC seconds"
        ));
    }
    Ok(parsed)
}

fn is_lower_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn load_best_cached_release(
    cache_dir: &Path,
    embedded: &ValidatedRelease,
) -> (Option<ValidatedRelease>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut paths = match cache_release_paths(cache_dir) {
        Ok(paths) => paths,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (None, warnings);
        }
        Err(error) => {
            warnings.push(format!(
                "could not inspect managed provider catalog {}: {error}",
                cache_dir.display()
            ));
            return (None, warnings);
        }
    };
    sort_cache_release_paths(&mut paths);
    if paths.len() > MAX_CACHE_FILES {
        warnings.push(format!(
            "managed provider catalog contains more than {MAX_CACHE_FILES} release files; checking the newest names only"
        ));
        paths.truncate(MAX_CACHE_FILES);
    }
    let mut best: Option<ValidatedRelease> = None;
    for path in paths {
        match read_cached_release(&path) {
            Ok(release) => consider_cached_release(release, embedded, &mut best, &mut warnings),
            Err(error) => warnings.push(format!(
                "ignored managed provider catalog {}: {error}",
                path.display()
            )),
        }
    }
    (best, warnings)
}

fn cache_release_paths(cache_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    fs::read_dir(cache_dir)?
        .filter_map(|entry| match entry {
            Ok(entry) => {
                let path = entry.path();
                cached_release_time(&path)?;
                match entry.file_type() {
                    Ok(file_type) if file_type.is_file() => Some(Ok(path)),
                    Ok(_) => None,
                    Err(error) => Some(Err(error)),
                }
            }
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn cached_release_time(path: &Path) -> Option<DateTime<Utc>> {
    let name = path.file_name()?.to_str()?.strip_suffix(".json")?;
    let (timestamp, digest) = name.strip_prefix("catalog-v1-")?.split_once('-')?;
    if timestamp.len() != 16
        || timestamp.as_bytes().get(8) != Some(&b't')
        || timestamp.as_bytes().get(15) != Some(&b'z')
        || !is_lower_hex_digest(digest)
    {
        return None;
    }
    NaiveDateTime::parse_from_str(timestamp, "%Y%m%dt%H%M%Sz")
        .ok()
        .map(|value| value.and_utc())
}

fn sort_cache_release_paths(paths: &mut [PathBuf]) {
    paths.sort_by(|left, right| {
        cached_release_time(right)
            .cmp(&cached_release_time(left))
            .then_with(|| right.file_name().cmp(&left.file_name()))
    });
}

fn maintain_cache(cache_dir: &Path, protected_release_id: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Err(error) = prune_cached_releases(cache_dir, protected_release_id, &mut warnings) {
        if error.kind() != std::io::ErrorKind::NotFound {
            warnings.push(format!(
                "could not inspect managed provider catalog cache for pruning: {error}"
            ));
        }
    }
    if let Err(error) = sweep_stale_temp_files(cache_dir, SystemTime::now(), &mut warnings) {
        if error.kind() != std::io::ErrorKind::NotFound {
            warnings.push(format!(
                "could not inspect managed provider catalog temporary files: {error}"
            ));
        }
    }
    warnings
}

fn prune_cached_releases(
    cache_dir: &Path,
    protected_release_id: &str,
    warnings: &mut Vec<String>,
) -> std::io::Result<()> {
    let mut paths = cache_release_paths(cache_dir)?;
    sort_cache_release_paths(&mut paths);
    let protected_name = format!("{protected_release_id}.json");
    let protected_present = paths.iter().any(|path| {
        path.file_name().and_then(|name| name.to_str()) == Some(protected_name.as_str())
    });
    let mut other_slots = RETAINED_CACHE_RELEASES.saturating_sub(usize::from(protected_present));
    for path in paths {
        let is_protected =
            path.file_name().and_then(|name| name.to_str()) == Some(protected_name.as_str());
        if is_protected {
            continue;
        }
        if other_slots > 0 {
            other_slots -= 1;
            continue;
        }
        if let Err(error) = fs::remove_file(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                warnings.push(format!(
                    "could not prune managed provider catalog {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

fn sweep_stale_temp_files(
    cache_dir: &Path,
    now: SystemTime,
    warnings: &mut Vec<String>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(cache_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !is_catalog_temp_name(name) {
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => metadata,
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                warnings.push(format!(
                    "could not inspect managed provider catalog temporary file {}: {error}",
                    path.display()
                ));
                continue;
            }
        };
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(error) => {
                warnings.push(format!(
                    "could not inspect managed provider catalog temporary file {}: {error}",
                    path.display()
                ));
                continue;
            }
        };
        let is_stale = now
            .duration_since(modified)
            .is_ok_and(|age| age >= STALE_TEMP_FILE_AGE);
        if is_stale {
            if let Err(error) = fs::remove_file(&path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    warnings.push(format!(
                        "could not remove stale managed provider catalog temporary file {}: {error}",
                        path.display()
                    ));
                }
            }
        }
    }
    Ok(())
}

fn is_catalog_temp_name(name: &str) -> bool {
    let Some(body) = name
        .strip_prefix(".provider-catalog-")
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    let Some((pid, sequence)) = body.split_once('-') else {
        return false;
    };
    let is_canonical_decimal = |value: &str| {
        !value.is_empty()
            && value.bytes().all(|byte| byte.is_ascii_digit())
            && (value == "0" || !value.starts_with('0'))
    };
    is_canonical_decimal(pid)
        && is_canonical_decimal(sequence)
        && pid.parse::<u32>().is_ok()
        && sequence.parse::<u64>().is_ok()
}

fn consider_cached_release(
    release: ValidatedRelease,
    embedded: &ValidatedRelease,
    best: &mut Option<ValidatedRelease>,
    warnings: &mut Vec<String>,
) {
    let baseline = best.as_ref().unwrap_or(embedded);
    match compare_releases(&release.manifest.release_id, release.generated_at, baseline) {
        Ok(ReleaseOrder::Newer) => *best = Some(release),
        Ok(ReleaseOrder::Same | ReleaseOrder::Older) => {}
        Err(error) => warnings.push(format!(
            "ignored managed provider catalog {}: {error}",
            release.manifest.release_id
        )),
    }
}

fn read_cached_release(path: &Path) -> Result<ValidatedRelease> {
    let bytes = read_bounded_file(path, CACHE_BUNDLE_LIMIT_BYTES)?;
    let bundle: CachedRelease = serde_json::from_slice(&bytes)
        .context("managed provider catalog bundle is not strict JSON")?;
    if bundle.schema_version != CACHE_SCHEMA_VERSION {
        return Err(anyhow!("unsupported managed catalog cache schema"));
    }
    let expected_name = format!("{}.json", bundle.manifest.release_id);
    if path.file_name().and_then(|value| value.to_str()) != Some(&expected_name) {
        return Err(anyhow!("cache filename does not match its release id"));
    }
    validate_manifest(&bundle.manifest)?;
    validate_release(bundle.manifest, bundle.catalog_json.into_bytes())
}

fn read_bounded_file(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    if file.metadata()?.len() > maximum {
        return Err(anyhow!("file exceeds {maximum} byte limit"));
    }
    read_bounded(file, maximum, &path.display().to_string())
}

fn write_cached_release(cache_dir: &Path, release: &ValidatedRelease) -> Result<()> {
    fs::create_dir_all(cache_dir).with_context(|| {
        format!(
            "could not create managed provider catalog directory {}",
            cache_dir.display()
        )
    })?;
    let bundle = CachedRelease {
        schema_version: CACHE_SCHEMA_VERSION,
        manifest: release.manifest.clone(),
        catalog_json: release.catalog_json.clone(),
    };
    let mut bytes = serde_json::to_vec_pretty(&bundle)?;
    bytes.push(b'\n');
    if bytes.len() as u64 > CACHE_BUNDLE_LIMIT_BYTES {
        return Err(anyhow!(
            "managed provider catalog cache bundle is too large"
        ));
    }
    let target = cache_dir.join(format!("{}.json", release.manifest.release_id));
    write_atomic(cache_dir, &target, &bytes)
}

fn write_refresh_state(
    cache_dir: &Path,
    now: DateTime<Utc>,
    outcome: RefreshStateOutcome,
    release_id: Option<String>,
) -> Result<()> {
    fs::create_dir_all(cache_dir)?;
    let state = RefreshState {
        schema_version: CACHE_SCHEMA_VERSION,
        attempted_at: now.to_rfc3339_opts(SecondsFormat::Secs, true),
        outcome,
        release_id,
    };
    let mut bytes = serde_json::to_vec_pretty(&state)?;
    bytes.push(b'\n');
    write_atomic(cache_dir, &cache_dir.join(REFRESH_STATE_FILE), &bytes)
}

fn write_atomic(parent: &Path, target: &Path, bytes: &[u8]) -> Result<()> {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".provider-catalog-{}-{sequence}.tmp",
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, target)?;
        sync_parent_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.with_context(|| format!("could not atomically write {}", target.display()))
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<()> {
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<()> {
    // Windows does not expose a portable directory handle through File::open.
    // The file itself is synced before the atomic rename above.
    Ok(())
}

fn release_catalog_url(release_id: &str) -> String {
    format!("{RELEASE_DOWNLOAD_ROOT}{release_id}/catalog-v1.json")
}

fn fetch_github_asset(source: &str, maximum: u64) -> Result<Vec<u8>> {
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(20))
        .build();
    let deadline = Instant::now() + HTTP_DEADLINE;
    let mut current = Url::parse(source).context("invalid provider catalog URL")?;
    validate_github_url(&current)?;
    for redirect_count in 0..=MAX_REDIRECTS {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow!("provider catalog request timed out"))?;
        let response = match agent.get(current.as_str()).timeout(remaining).call() {
            Ok(response) => response,
            Err(ureq::Error::Status(_, response)) => response,
            Err(error) => {
                return Err(anyhow!(
                    "provider catalog request to GitHub failed: {}",
                    error.kind()
                ));
            }
        };
        if (300..400).contains(&response.status()) {
            if redirect_count == MAX_REDIRECTS {
                return Err(anyhow!("provider catalog redirect limit exceeded"));
            }
            let location = response
                .header("Location")
                .ok_or_else(|| anyhow!("provider catalog redirect omitted Location"))?;
            current = current
                .join(location)
                .context("invalid provider catalog redirect URL")?;
            validate_github_url(&current)?;
            continue;
        }
        if response.status() != 200 {
            return Err(anyhow!(
                "provider catalog request returned HTTP {}",
                response.status()
            ));
        }
        validate_content_length(&response, maximum)?;
        return read_bounded(
            response.into_reader(),
            maximum,
            "GitHub provider catalog response",
        );
    }
    Err(anyhow!("provider catalog redirect limit exceeded"))
}

fn validate_content_length(response: &ureq::Response, maximum: u64) -> Result<()> {
    let Some(value) = response.header("Content-Length") else {
        return Ok(());
    };
    let length = value
        .parse::<u64>()
        .context("provider catalog Content-Length is invalid")?;
    if length > maximum {
        return Err(anyhow!(
            "provider catalog response exceeds {maximum} byte limit"
        ));
    }
    Ok(())
}

fn read_bounded(mut reader: impl Read, maximum: u64, source: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("could not read provider catalog from {source}"))?;
    if bytes.len() as u64 > maximum {
        return Err(anyhow!(
            "provider catalog response exceeds {maximum} byte limit"
        ));
    }
    Ok(bytes)
}

fn validate_github_url(url: &Url) -> Result<()> {
    let allowed_host = matches!(
        url.host_str(),
        Some(
            "github.com"
                | "release-assets.githubusercontent.com"
                | "objects.githubusercontent.com"
                | "github-releases.githubusercontent.com"
        )
    );
    if url.scheme() != "https"
        || !allowed_host
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|port| port != 443)
    {
        return Err(anyhow!(
            "provider catalog URL is outside the GitHub release boundary"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn embedded_release_authenticates_and_contains_kimi_k3() {
        let release = embedded_release();
        assert_eq!(env!("CARGO_PKG_VERSION"), "0.1.3");
        assert!(release
            .catalog
            .provider("openrouter")
            .expect("openrouter")
            .models()
            .any(|model| model.id() == "moonshotai/kimi-k3"));
    }

    #[test]
    fn newer_catalog_protocol_is_rejected_before_artifact_use() {
        let mut manifest = embedded_release().manifest;
        manifest.minimum_euler_version = "0.1.4".to_owned();

        let error = ensure_compatible(&manifest).expect_err("future protocol must fail");

        assert!(error.to_string().contains("requires Euler 0.1.4"));
        assert!(error.to_string().contains("this binary is 0.1.3"));
    }

    #[test]
    fn manifest_identity_rejects_sidecar_tampering() {
        let mut manifest: serde_json::Value =
            serde_json::from_str(EMBEDDED_MANIFEST_JSON).expect("manifest");
        manifest["minimum_euler_version"] = serde_json::json!("0.1.0");
        let bytes = serde_json::to_vec(&manifest).expect("encode");
        let error = parse_and_validate_manifest(&bytes).expect_err("tampering must fail");
        assert!(error.to_string().contains("does not authenticate"));
    }

    #[test]
    fn corrupt_cached_release_falls_back_to_embedded() {
        let temp = tempfile::tempdir().expect("temp dir");
        let release = release_after_embedded_hours(1);
        let path = temp
            .path()
            .join(format!("{}.json", release.manifest.release_id));
        fs::write(path, b"not json").expect("write");

        let load = load_managed_catalog(Some(temp.path()));

        assert!(!load.from_cache);
        assert_eq!(load.release_id, embedded_release().manifest.release_id);
        assert_eq!(load.warnings.len(), 1);
    }

    #[test]
    fn valid_newer_cached_release_replaces_embedded_membership() {
        let temp = tempfile::tempdir().expect("temp dir");
        let candidate = release_after_embedded_hours(1);
        write_cached_release(temp.path(), &candidate).expect("cache");

        let load = load_managed_catalog(Some(temp.path()));

        assert!(load.from_cache);
        assert_eq!(load.release_id, candidate.manifest.release_id);
    }

    #[test]
    fn refresh_installs_only_a_newer_valid_release() {
        let temp = tempfile::tempdir().expect("temp dir");
        let cache_dir = temp
            .path()
            .join("fresh-home")
            .join(".euler")
            .join("catalogs")
            .join("provider-v1");
        let candidate = release_after_embedded_hours(1);
        let manifest = canonical_manifest_bytes(&candidate.manifest);
        let catalog = candidate.catalog_json.as_bytes().to_vec();
        let expected_catalog_url = release_catalog_url(&candidate.manifest.release_id);
        let mut responses = VecDeque::from([
            (LATEST_MANIFEST_URL.to_owned(), manifest),
            (expected_catalog_url, catalog),
        ]);

        let report = refresh_managed_catalog_with(&cache_dir, test_now(), |url, _| {
            let (expected, bytes) = responses.pop_front().expect("expected fetch");
            assert_eq!(url, expected);
            Ok(bytes)
        })
        .expect("refresh");

        assert!(report.outcome.was_updated());
        assert!(responses.is_empty());
        assert!(load_managed_catalog(Some(&cache_dir)).from_cache);
    }

    #[test]
    fn successful_refresh_bounds_cache_and_sweeps_only_stale_owned_temps() {
        let temp = tempfile::tempdir().expect("temp dir");
        let releases = [1, 2, 3, 4, 5].map(release_after_embedded_hours);
        for release in &releases {
            write_cached_release(temp.path(), release).expect("cached release");
        }
        let current = releases.last().expect("current release");
        let manifest = canonical_manifest_bytes(&current.manifest);

        let stale_temp = temp.path().join(".provider-catalog-100-0.tmp");
        fs::write(&stale_temp, b"stale").expect("stale temp");
        let stale_time = SystemTime::now()
            .checked_sub(STALE_TEMP_FILE_AGE + Duration::from_secs(1))
            .expect("stale time");
        File::options()
            .write(true)
            .open(&stale_temp)
            .expect("open stale temp")
            .set_times(std::fs::FileTimes::new().set_modified(stale_time))
            .expect("age stale temp");
        let active_temp = temp.path().join(".provider-catalog-100-1.tmp");
        fs::write(&active_temp, b"active").expect("active temp");
        let malformed_temp = temp.path().join(".provider-catalog-not-ours.tmp");
        fs::write(&malformed_temp, b"malformed").expect("malformed temp");
        File::options()
            .write(true)
            .open(&malformed_temp)
            .expect("open malformed temp")
            .set_times(std::fs::FileTimes::new().set_modified(stale_time))
            .expect("age malformed temp");
        let unrelated_temp = temp.path().join("unrelated.tmp");
        fs::write(&unrelated_temp, b"unrelated").expect("unrelated temp");

        let mut calls = 0;
        let report = refresh_managed_catalog_with(temp.path(), test_now(), |url, _| {
            calls += 1;
            assert_eq!(url, LATEST_MANIFEST_URL);
            Ok(manifest.clone())
        })
        .expect("current refresh");

        assert_eq!(calls, 1);
        assert!(!report.outcome.was_updated());
        assert!(report.warnings.is_empty());
        let retained = cache_release_paths(temp.path()).expect("retained releases");
        assert_eq!(retained.len(), RETAINED_CACHE_RELEASES);
        for release in &releases[2..] {
            assert!(retained.contains(
                &temp
                    .path()
                    .join(format!("{}.json", release.manifest.release_id))
            ));
        }
        assert!(!stale_temp.exists());
        assert!(active_temp.exists());
        assert!(malformed_temp.exists());
        assert!(unrelated_temp.exists());
    }

    #[test]
    fn pruning_preserves_an_older_selected_release_and_the_two_newest_others() {
        let temp = tempfile::tempdir().expect("temp dir");
        let releases = [1, 2, 3, 4, 5].map(release_after_embedded_hours);
        for release in &releases {
            write_cached_release(temp.path(), release).expect("cached release");
        }
        let protected = releases.first().expect("protected release");
        let mut warnings = Vec::new();

        prune_cached_releases(temp.path(), &protected.manifest.release_id, &mut warnings)
            .expect("prune");

        assert!(warnings.is_empty());
        let retained = cache_release_paths(temp.path()).expect("retained releases");
        assert_eq!(retained.len(), RETAINED_CACHE_RELEASES);
        for release in [&releases[0], &releases[3], &releases[4]] {
            assert!(retained.contains(
                &temp
                    .path()
                    .join(format!("{}.json", release.manifest.release_id))
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn cache_maintenance_does_not_treat_symlinks_or_directories_as_bundles() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temp dir");
        let target = temp.path().join("target");
        fs::write(&target, b"keep").expect("target");
        let symlink_release = release_after_embedded_hours(1);
        let symlink_path = temp
            .path()
            .join(format!("{}.json", symlink_release.manifest.release_id));
        symlink(&target, &symlink_path).expect("symlink");
        let directory_release = release_after_embedded_hours(2);
        let directory_path = temp
            .path()
            .join(format!("{}.json", directory_release.manifest.release_id));
        fs::create_dir(&directory_path).expect("directory");

        let mut warnings = Vec::new();
        prune_cached_releases(temp.path(), "embedded", &mut warnings).expect("prune");

        assert!(warnings.is_empty());
        assert!(cache_release_paths(temp.path())
            .expect("release paths")
            .is_empty());
        assert!(fs::symlink_metadata(&symlink_path)
            .expect("symlink remains")
            .file_type()
            .is_symlink());
        assert!(directory_path.is_dir());
        assert_eq!(fs::read(target).expect("target remains"), b"keep");
    }

    #[test]
    fn failed_digest_preserves_last_known_good_release() {
        let temp = tempfile::tempdir().expect("temp dir");
        let baseline = release_after_embedded_hours(1);
        write_cached_release(temp.path(), &baseline).expect("baseline");
        let candidate = release_after_embedded_hours(2);
        let manifest = canonical_manifest_bytes(&candidate.manifest);
        let mut calls = 0;

        let result = refresh_managed_catalog_with(temp.path(), test_now(), |_, _| {
            calls += 1;
            Ok(if calls == 1 {
                manifest.clone()
            } else {
                b"tampered".to_vec()
            })
        });

        assert!(result.is_err());
        assert_eq!(
            load_managed_catalog(Some(temp.path())).release_id,
            baseline.manifest.release_id
        );
    }

    #[test]
    fn refresh_refuses_a_remote_downgrade_without_fetching_catalog() {
        let temp = tempfile::tempdir().expect("temp dir");
        let newer = release_after_embedded_hours(2);
        write_cached_release(temp.path(), &newer).expect("newer cache");
        let older = release_after_embedded_hours(1);
        let manifest = canonical_manifest_bytes(&older.manifest);
        let mut calls = 0;

        let error = refresh_managed_catalog_with(temp.path(), test_now(), |_, _| {
            calls += 1;
            Ok(manifest.clone())
        })
        .expect_err("downgrade must fail");

        assert_eq!(calls, 1);
        assert!(error.to_string().contains("downgrade"));
        assert_eq!(
            load_managed_catalog(Some(temp.path())).release_id,
            newer.manifest.release_id
        );
    }

    #[test]
    fn refresh_refuses_an_implausibly_future_release_without_fetching_catalog() {
        let temp = tempfile::tempdir().expect("temp dir");
        let candidate = release_at_datetime(
            test_now()
                + chrono::Duration::from_std(MAX_RELEASE_CLOCK_SKEW).expect("clock skew")
                + chrono::Duration::seconds(1),
        );
        let manifest = canonical_manifest_bytes(&candidate.manifest);
        let mut calls = 0;

        let error = refresh_managed_catalog_with(temp.path(), test_now(), |_, _| {
            calls += 1;
            Ok(manifest.clone())
        })
        .expect_err("future release must fail");

        assert_eq!(calls, 1);
        assert!(error.to_string().contains("future"));
        assert!(!load_managed_catalog(Some(temp.path())).from_cache);
    }

    #[test]
    fn automatic_refresh_uses_shorter_backoff_after_failure() {
        let temp = tempfile::tempdir().expect("temp dir");
        let now = DateTime::parse_from_rfc3339("2026-07-20T12:00:00Z")
            .expect("time")
            .with_timezone(&Utc);
        write_refresh_state(temp.path(), now, RefreshStateOutcome::Failed, None).expect("state");
        assert!(!automatic_refresh_due_at(
            temp.path(),
            now + chrono::Duration::minutes(59)
        ));
        assert!(automatic_refresh_due_at(
            temp.path(),
            now + chrono::Duration::minutes(60)
        ));
    }

    #[test]
    fn malformed_or_oversized_refresh_state_fails_open_to_a_check() {
        let temp = tempfile::tempdir().expect("temp dir");
        let state = temp.path().join(REFRESH_STATE_FILE);
        fs::write(&state, b"not json").expect("malformed state");
        assert!(automatic_refresh_due_at(temp.path(), test_now()));

        fs::write(&state, vec![b'x'; MANIFEST_LIMIT_BYTES as usize + 1]).expect("oversized state");
        assert!(automatic_refresh_due_at(temp.path(), test_now()));
    }

    #[test]
    fn managed_catalog_is_replaced_before_user_metadata_overlay() {
        let temp = tempfile::tempdir().expect("temp dir");
        let model_path = temp.path().join(".euler").join("models.json");
        let cache_dir = managed_catalog_dir_for_model_path(&model_path);
        let candidate = release_after_embedded_hours(1);
        write_cached_release(&cache_dir, &candidate).expect("managed catalog");
        fs::write(
            &model_path,
            r#"{
              "version": 1,
              "providers": {
                "openrouter": {
                  "default_model": "moonshotai/kimi-k3",
                  "models": [{
                    "id": "moonshotai/kimi-k3",
                    "display_name": "My Kimi K3"
                  }]
                }
              }
            }"#,
        )
        .expect("user overlay");

        let load = crate::model_catalog::load_model_catalog(Some(&model_path));
        let openrouter = load.catalog.provider("openrouter").expect("openrouter");
        let kimi = openrouter
            .models()
            .find(|model| model.id() == "moonshotai/kimi-k3")
            .expect("Kimi K3");

        assert!(load.managed);
        assert_eq!(openrouter.default_model(), "moonshotai/kimi-k3");
        assert_eq!(kimi.display_name(), "My Kimi K3");
        assert_eq!(
            kimi.source(),
            euler_provider::catalog::ModelDescriptorSource::Local
        );
    }

    #[test]
    fn legacy_machine_generated_overlay_is_ignored_at_load_boundary() {
        let temp = tempfile::tempdir().expect("temp dir");
        let model_path = temp.path().join("models.json");
        fs::write(
            &model_path,
            r#"{
              "version": 1,
              "generated_by": "euler models refresh",
              "providers": {
                "openrouter": { "default_model": "legacy/model" }
              }
            }"#,
        )
        .expect("legacy overlay");

        let load = crate::model_catalog::load_model_catalog(Some(&model_path));

        assert_ne!(
            load.catalog
                .default_model_for_provider("openrouter")
                .expect("default"),
            "legacy/model"
        );
        assert!(load
            .warnings
            .iter()
            .any(|warning| warning.contains("legacy machine-generated")));

        let candidate = release_after_embedded_hours(1);
        let cache_dir = managed_catalog_dir_for_model_path(&model_path);
        write_cached_release(&cache_dir, &candidate).expect("managed catalog");
        let managed = crate::model_catalog::load_model_catalog(Some(&model_path));

        assert!(managed.managed);
        assert_eq!(managed.release_id, candidate.manifest.release_id);
        assert_ne!(
            managed
                .catalog
                .default_model_for_provider("openrouter")
                .expect("default"),
            "legacy/model"
        );
    }

    #[test]
    fn github_redirect_boundary_rejects_non_https_and_foreign_hosts() {
        for value in [
            "http://github.com/release",
            "https://example.com/release",
            "https://github.com@evil.example/release",
            "https://github.com:444/release",
        ] {
            let url = Url::parse(value).expect("URL");
            assert!(validate_github_url(&url).is_err(), "{value}");
        }
        assert!(validate_github_url(
            &Url::parse("https://release-assets.githubusercontent.com/asset?sig=x").expect("URL")
        )
        .is_ok());
    }

    #[test]
    fn bounded_reader_rejects_one_byte_over_limit() {
        let error = read_bounded(&b"12345"[..], 4, "test").expect_err("oversize");
        assert!(error.to_string().contains("exceeds 4 byte limit"));
    }

    #[test]
    fn temporary_file_matcher_accepts_only_the_atomic_writer_shape() {
        assert!(is_catalog_temp_name(".provider-catalog-123-0.tmp"));
        for name in [
            ".provider-catalog-anything.tmp",
            ".provider-catalog-123.tmp",
            ".provider-catalog-0123-0.tmp",
            ".provider-catalog-123-00.tmp",
            ".provider-catalog-123-0.tmp.extra",
        ] {
            assert!(!is_catalog_temp_name(name), "{name}");
        }
    }

    fn release_at(generated_at: &str) -> ValidatedRelease {
        let embedded = embedded_release();
        let mut manifest = embedded.manifest;
        manifest.generated_at = generated_at.to_owned();
        let parsed = parse_generated_at(generated_at).expect("generated at");
        manifest.release_id = release_id(&manifest, parsed).expect("release id");
        validate_release(manifest, embedded.catalog_json.into_bytes()).expect("release")
    }

    fn release_at_datetime(generated_at: DateTime<Utc>) -> ValidatedRelease {
        release_at(&generated_at.to_rfc3339_opts(SecondsFormat::Secs, true))
    }

    fn release_after_embedded_hours(hours: i64) -> ValidatedRelease {
        release_at_datetime(embedded_release().generated_at + chrono::Duration::hours(hours))
    }

    fn canonical_manifest_bytes(manifest: &ReleaseManifest) -> Vec<u8> {
        let mut bytes = serde_json::to_vec_pretty(manifest).expect("manifest JSON");
        bytes.push(b'\n');
        bytes
    }

    fn test_now() -> DateTime<Utc> {
        embedded_release().generated_at + chrono::Duration::hours(12)
    }
}
