//! The gateway discovery cache and its refresh for `"discovery": true`
//! providers (ADR 0025 section 3). Startup routes from the cache in
//! `<euler home>/cache/provider-discovery/<provider id>.json`; a missing cache
//! is fetched before launch and a cache older than a day is refreshed in the
//! background for later sessions.

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use euler_core::{FoldedSession, ModelTarget};
use euler_event::{EventEnvelope, EventKind};
use euler_provider::custom_provider::CustomOpenAiProvider;
use euler_provider::gateway_discovery::GatewayDiscovery;
use euler_provider::provider_config::{
    parse_discovered_models, ApiFamily, CustomProviderConfig, DiscoveredModels, DiscoverySource,
    ProviderConfigRegistry,
};
use euler_provider::ProviderSet;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const CACHE_FORMAT_VERSION: u64 = 1;
const STALE_AFTER_HOURS: i64 = 24;

/// Warnings from background refreshes, printed once when the command exits
/// (the TUI owns the terminal while it runs).
static BACKGROUND_WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The discovery cache directory for the Euler home holding `providers.json`.
pub(crate) fn cache_dir(provider_config_path: &Path) -> PathBuf {
    provider_config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("cache")
        .join("provider-discovery")
}

fn cache_path(cache_dir: &Path, provider_id: &str) -> PathBuf {
    cache_dir.join(format!("{provider_id}.json"))
}

/// A provider's cached discovery, with the time it was fetched.
pub(crate) struct CachedDiscovery {
    pub(crate) discovered: DiscoveredModels,
    fetched_at: DateTime<Utc>,
}

/// Reads a provider's cache. A missing, unreadable, or malformed cache, or
/// one whose format version or base URL does not match the entry, is a miss.
pub(crate) fn read_cache(
    cache_dir: &Path,
    provider_id: &str,
    base_url: &str,
) -> Option<CachedDiscovery> {
    let bytes = fs::read(cache_path(cache_dir, provider_id)).ok()?;
    let cache: Value = serde_json::from_slice(&bytes).ok()?;
    if cache.get("version").and_then(Value::as_u64) != Some(CACHE_FORMAT_VERSION)
        || cache.get("base_url").and_then(Value::as_str) != Some(base_url)
    {
        return None;
    }
    let fetched_at_text = cache.get("fetched_at")?.as_str()?;
    let fetched_at = DateTime::parse_from_rfc3339(fetched_at_text)
        .ok()?
        .with_timezone(&Utc);
    let endpoint = cache.get("source")?.as_str()?;
    let models = parse_discovered_models(cache.get("models")?.as_array()?);
    Some(CachedDiscovery {
        discovered: DiscoveredModels {
            source: DiscoverySource {
                endpoint: endpoint.to_owned(),
                fetched_at: fetched_at_text.to_owned(),
                cache_sha256: format!("{:x}", Sha256::digest(&bytes)),
            },
            models,
        },
        fetched_at,
    })
}

impl CachedDiscovery {
    fn is_stale(&self, now: DateTime<Utc>) -> bool {
        let age = now.signed_duration_since(self.fetched_at);
        age < chrono::Duration::zero() || age >= chrono::Duration::hours(STALE_AFTER_HOURS)
    }
}

/// Writes a successful discovery atomically, owner-only. Concurrent writers
/// are last-writer-wins.
fn write_cache(
    cache_dir: &Path,
    provider_id: &str,
    base_url: &str,
    discovery: &GatewayDiscovery,
    now: DateTime<Utc>,
) -> Result<()> {
    create_private_dir(cache_dir)?;
    let cache = json!({
        "version": CACHE_FORMAT_VERSION,
        "base_url": base_url,
        "fetched_at": now.to_rfc3339_opts(SecondsFormat::Secs, true),
        "source": discovery.endpoint,
        "models": discovery.models.iter().map(|model| model.entry_json()).collect::<Vec<_>>(),
    });
    let mut bytes = serde_json::to_vec_pretty(&cache)?;
    bytes.push(b'\n');
    let target = cache_path(cache_dir, provider_id);
    let temp = cache_dir.join(format!(
        ".{provider_id}-{}-{}.tmp",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, &target)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written.with_context(|| format!("could not write {}", target.display()))
}

fn create_private_dir(dir: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .with_context(|| format!("could not create {}", dir.display()))
}

/// One provider's successful refresh, compared with the cache it replaced.
pub(crate) struct RefreshOutcome {
    pub(crate) discovery: GatewayDiscovery,
    pub(crate) added: Vec<String>,
    pub(crate) removed: Vec<String>,
}

/// Runs discovery for `provider` and writes its cache. A failed or empty
/// discovery leaves the cache as it was.
pub(crate) fn refresh_provider(
    cache_dir: &Path,
    provider: &CustomProviderConfig,
) -> Result<RefreshOutcome, String> {
    let discovery = CustomOpenAiProvider::from_config(provider.clone())
        .map_err(|error| error.to_string())?
        .with_stored_api_key(Arc::new(crate::cli::providers::stored_custom_api_key))
        .discover_models()?;
    let previous = read_cache(cache_dir, &provider.id, &provider.base_url)
        .map(|cache| model_ids(&cache.discovered.models))
        .unwrap_or_default();
    let current = model_ids(&discovery.models);
    write_cache(
        cache_dir,
        &provider.id,
        &provider.base_url,
        &discovery,
        Utc::now(),
    )
    .map_err(|error| format!("{error:#}"))?;
    Ok(RefreshOutcome {
        added: current.difference(&previous).cloned().collect(),
        removed: previous.difference(&current).cloned().collect(),
        discovery,
    })
}

fn model_ids(models: &[euler_provider::provider_config::CustomModelConfig]) -> BTreeSet<String> {
    models.iter().map(|model| model.id.clone()).collect()
}

/// `42 models from /model/info: 12 anthropic_messages, 3 openai_responses,
/// 27 openai_chat_completions`.
pub(crate) fn family_counts(discovery: &GatewayDiscovery) -> String {
    let counts = [
        ApiFamily::AnthropicMessages,
        ApiFamily::OpenAiResponses,
        ApiFamily::OpenAiChatCompletions,
    ]
    .into_iter()
    .filter_map(|family| {
        let count = discovery
            .models
            .iter()
            .filter(|model| model.api_family == family)
            .count();
        (count > 0).then(|| format!("{count} {}", family.as_str()))
    })
    .collect::<Vec<_>>();
    format!(
        "{} models from {}: {}",
        discovery.models.len(),
        discovery.endpoint,
        counts.join(", ")
    )
}

pub(crate) fn failure_warning(provider_id: &str, error: &str) -> String {
    format!("discovery for provider `{provider_id}` failed: {error}; run `euler models refresh` to retry")
}

/// Brings discovery caches up to date before a session launches: a provider
/// with no cache is fetched now (all of them in parallel, each under the
/// 10-second discovery deadline), and a stale cache is refreshed in the
/// background while this launch routes from it. Returns warnings to print.
pub(crate) fn refresh_at_launch(provider_config_path: &Path) -> Vec<String> {
    let Ok(contents) = fs::read_to_string(provider_config_path) else {
        return Vec::new();
    };
    let (registry, _) = ProviderConfigRegistry::with_json(&contents);
    let cache_dir = cache_dir(provider_config_path);
    let now = Utc::now();
    let mut foreground = Vec::new();
    for provider in registry.providers().filter(|provider| provider.discovery) {
        match read_cache(&cache_dir, &provider.id, &provider.base_url) {
            None => foreground.push(provider.clone()),
            Some(cache) if cache.is_stale(now) => {
                let (cache_dir, provider) = (cache_dir.clone(), provider.clone());
                std::thread::spawn(move || {
                    if let Err(error) = refresh_provider(&cache_dir, &provider) {
                        BACKGROUND_WARNINGS
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(failure_warning(&provider.id, &error));
                    }
                });
            }
            Some(_) => {}
        }
    }
    let handles = foreground
        .into_iter()
        .map(|provider| {
            let cache_dir = cache_dir.clone();
            std::thread::spawn(move || {
                refresh_provider(&cache_dir, &provider)
                    .err()
                    .map(|error| failure_warning(&provider.id, &error))
            })
        })
        .collect::<Vec<_>>();
    handles
        .into_iter()
        .filter_map(|handle| handle.join().ok().flatten())
        .collect()
}

/// Background refresh warnings collected so far, each returned once.
pub(crate) fn take_background_warnings() -> Vec<String> {
    std::mem::take(
        &mut *BACKGROUND_WARNINGS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// `euler models refresh` for discovery providers: refreshes each one and
/// reports its counts per family, the ids added and removed, and failures.
/// Returns whether every refresh succeeded.
pub(crate) fn refresh_all(
    provider_config_path: &Path,
    registry: &ProviderConfigRegistry,
    mut stdout: impl Write,
    mut stderr: impl Write,
) -> Result<bool> {
    let cache_dir = cache_dir(provider_config_path);
    let mut all_succeeded = true;
    for provider in registry.providers().filter(|provider| provider.discovery) {
        match refresh_provider(&cache_dir, provider) {
            Ok(outcome) => write_refresh_outcome(&provider.id, &outcome, &mut stdout, &mut stderr)?,
            Err(error) => {
                all_succeeded = false;
                writeln!(stderr, "warning: {}", failure_warning(&provider.id, &error))?;
            }
        }
    }
    Ok(all_succeeded)
}

pub(crate) fn write_refresh_outcome(
    provider_id: &str,
    outcome: &RefreshOutcome,
    mut stdout: impl Write,
    mut stderr: impl Write,
) -> Result<()> {
    writeln!(
        stdout,
        "{provider_id}: discovered {}",
        family_counts(&outcome.discovery)
    )?;
    if !outcome.added.is_empty() {
        writeln!(stdout, "  added: {}", outcome.added.join(", "))?;
    }
    if !outcome.removed.is_empty() {
        writeln!(stdout, "  removed: {}", outcome.removed.join(", "))?;
    }
    for note in &outcome.discovery.notes {
        writeln!(
            stderr,
            "warning: discovery for provider `{provider_id}` {note}"
        )?;
    }
    Ok(())
}

/// Prints [`resume_model_source_change`] for a resume about to proceed.
pub(crate) fn print_resume_model_source_change(folded: &FoldedSession, providers: &ProviderSet) {
    let target = &folded.active_target;
    let current = providers.model_source(&target.provider, &target.model);
    if let Some(change) = resume_model_source_change(&folded.events, target, current.as_ref()) {
        eprintln!("{change}");
    }
}

/// The resume report for a model source that changed since the session last
/// recorded one (`session.start`, `model.switched`, or `session.resumed`), as
/// after a discovery refresh. The resume proceeds either way.
pub(crate) fn resume_model_source_change(
    events: &[EventEnvelope],
    target: &ModelTarget,
    current: Option<&Value>,
) -> Option<String> {
    let recorded = events
        .iter()
        .rev()
        .find(|event| {
            matches!(
                event.kind.as_str(),
                EventKind::SESSION_START | EventKind::MODEL_SWITCHED | EventKind::SESSION_RESUMED
            )
        })
        .and_then(|event| event.payload.get("model_source"));
    if recorded == current {
        return None;
    }
    let model = format!("{}/{}", target.provider, target.model);
    Some(match current {
        Some(source) => format!(
            "note: the model list for {model} changed since this session last recorded it; now from {} fetched at {}",
            source.get("endpoint").and_then(Value::as_str).unwrap_or("its configuration"),
            source.get("fetched_at").and_then(Value::as_str).unwrap_or("an unknown time"),
        ),
        None => format!(
            "note: {model} is no longer a discovered model; it is sent as an unlisted model"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    const MODEL_INFO: &str = r#"{"data": [
        {"model_name": "claude-sonnet", "litellm_params": {"model": "bedrock/anthropic.claude-sonnet"},
         "model_info": {"litellm_provider": "bedrock", "mode": "chat", "max_input_tokens": 200000}},
        {"model_name": "gpt-4o", "litellm_params": {"model": "openai/gpt-4o"},
         "model_info": {"litellm_provider": "openai", "mode": "chat"}},
        {"model_name": "codex", "litellm_params": {"model": "openai/codex"},
         "model_info": {"litellm_provider": "openai", "mode": "responses"}}
    ]}"#;

    /// Answers every request with `status` and `body`, counting requests.
    fn spawn_gateway(status: u16, body: &'static str) -> (String, Arc<AtomicU64>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}/v1", listener.local_addr().expect("addr"));
        let requests = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut head = Vec::new();
                let mut byte = [0_u8; 1];
                while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                    head.push(byte[0]);
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (base_url, requests)
    }

    fn write_providers(home: &Path, base_url: &str) -> PathBuf {
        let path = home.join("providers.json");
        fs::write(
            &path,
            format!(
                r#"{{"providers": {{"gw": {{"api_family": "openai_chat_completions",
                "base_url": "{base_url}", "api_key": "literal-key", "auth_header": true,
                "discovery": true}}}}}}"#
            ),
        )
        .expect("providers.json");
        path
    }

    fn registry(path: &Path) -> ProviderConfigRegistry {
        crate::provider_config_runtime::load_provider_config(Some(path)).registry
    }

    #[test]
    fn launch_without_a_cache_fetches_and_routes_from_the_written_cache() {
        let home = tempfile::tempdir().expect("home");
        let (base_url, requests) = spawn_gateway(200, MODEL_INFO);
        let path = write_providers(home.path(), &base_url);

        let warnings = refresh_at_launch(&path);

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        let registry = registry(&path);
        let provider = registry.provider("gw").expect("gw");
        assert_eq!(
            provider.model_api_family("claude-sonnet"),
            ApiFamily::AnthropicMessages
        );
        assert_eq!(
            provider.model_api_family("codex"),
            ApiFamily::OpenAiResponses
        );
        let source = provider.model_source("gpt-4o").expect("discovered source");
        assert_eq!(source.endpoint, "/model/info");
        let cache = cache_path(&cache_dir(&path), "gw");
        assert_eq!(
            source.cache_sha256,
            format!("{:x}", Sha256::digest(fs::read(&cache).expect("cache")))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&cache)
                .expect("cache metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // A fresh cache is not fetched again.
        assert!(refresh_at_launch(&path).is_empty());
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_launch_fetch_warns_and_loads_the_provider_without_listed_models() {
        let home = tempfile::tempdir().expect("home");
        let (base_url, _requests) = spawn_gateway(503, "{}");
        let path = write_providers(home.path(), &base_url);
        let root = base_url.trim_end_matches("/v1");

        let warnings = refresh_at_launch(&path);

        assert_eq!(
            warnings,
            [format!(
                "discovery for provider `gw` failed: GET {root}/v1/models returned HTTP 503 (GET {root}/model/info returned HTTP 503); run `euler models refresh` to retry"
            )]
        );
        assert!(!cache_path(&cache_dir(&path), "gw").exists());
        assert!(registry(&path)
            .provider("gw")
            .expect("gw")
            .models
            .is_empty());
    }

    #[test]
    fn cache_for_another_base_url_or_format_version_or_truncated_is_a_miss_and_old_caches_are_stale(
    ) {
        let home = tempfile::tempdir().expect("home");
        let (base_url, _requests) = spawn_gateway(200, MODEL_INFO);
        let path = write_providers(home.path(), &base_url);
        assert!(refresh_at_launch(&path).is_empty());
        let dir = cache_dir(&path);
        let cache = read_cache(&dir, "gw", &base_url).expect("cache hit");
        let now = Utc::now();

        assert!(!cache.is_stale(now));
        assert!(cache.is_stale(now + chrono::Duration::hours(24)));
        assert!(read_cache(&dir, "gw", "https://other.example.com/v1").is_none());
        let file = cache_path(&dir, "gw");
        let mut contents: Value =
            serde_json::from_slice(&fs::read(&file).expect("cache")).expect("cache json");
        contents["version"] = json!(2);
        fs::write(&file, contents.to_string()).expect("rewrite cache");
        assert!(read_cache(&dir, "gw", &base_url).is_none());
        fs::write(&file, "{\"version\": 1, \"base_url\"").expect("truncate cache");
        assert!(read_cache(&dir, "gw", &base_url).is_none());
    }

    #[test]
    fn stale_cache_routes_this_launch_and_holds_a_background_failure_for_exit_once() {
        let home = tempfile::tempdir().expect("home");
        let (base_url, requests) = spawn_gateway(503, "{}");
        let path = write_providers(home.path(), &base_url);
        let dir = cache_dir(&path);
        let cached = GatewayDiscovery {
            endpoint: "/v1/models",
            models: parse_discovered_models(&[
                json!({"id": "gpt-4o", "api_family": "openai_chat_completions"}),
            ]),
            notes: Vec::new(),
        };
        let fetched_at = Utc::now() - chrono::Duration::hours(STALE_AFTER_HOURS + 1);
        write_cache(&dir, "gw", &base_url, &cached, fetched_at).expect("seed cache");
        let before = fs::read(cache_path(&dir, "gw")).expect("cache");

        let warnings = refresh_at_launch(&path);

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(registry(&path)
            .provider("gw")
            .expect("gw")
            .model_source("gpt-4o")
            .is_some());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let background = loop {
            let taken = take_background_warnings();
            if !taken.is_empty() || std::time::Instant::now() > deadline {
                break taken;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!(background.len(), 1, "{background:?}");
        assert!(
            background[0].starts_with("discovery for provider `gw` failed: GET "),
            "{background:?}"
        );
        assert!(take_background_warnings().is_empty());
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        assert_eq!(fs::read(cache_path(&dir, "gw")).expect("cache"), before);
    }

    #[test]
    fn models_refresh_reports_family_counts_and_added_and_removed_ids_and_keeps_cache_on_failure() {
        let home = tempfile::tempdir().expect("home");
        let (base_url, _requests) = spawn_gateway(200, MODEL_INFO);
        let path = write_providers(home.path(), &base_url);
        let dir = cache_dir(&path);
        let stale = GatewayDiscovery {
            endpoint: "/v1/models",
            models: parse_discovered_models(&[
                json!({"id": "gpt-4o", "api_family": "openai_chat_completions"}),
                json!({"id": "retired", "api_family": "openai_chat_completions"}),
            ]),
            notes: Vec::new(),
        };
        write_cache(&dir, "gw", &base_url, &stale, Utc::now()).expect("seed cache");
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());

        let succeeded =
            refresh_all(&path, &registry(&path), &mut stdout, &mut stderr).expect("refresh");

        assert!(succeeded);
        assert_eq!(
            String::from_utf8(stdout).expect("utf8"),
            "gw: discovered 3 models from /model/info: 1 anthropic_messages, 1 openai_responses, 1 openai_chat_completions\n  added: claude-sonnet, codex\n  removed: retired\n"
        );
        assert!(stderr.is_empty());

        let (down_url, _requests) = spawn_gateway(500, "{}");
        let down = write_providers(home.path(), &down_url);
        write_cache(&dir, "gw", &down_url, &stale, Utc::now()).expect("seed cache");
        let before = fs::read(cache_path(&dir, "gw")).expect("cache");
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());

        let succeeded =
            refresh_all(&down, &registry(&down), &mut stdout, &mut stderr).expect("refresh");

        assert!(!succeeded);
        assert!(String::from_utf8(stderr)
            .expect("utf8")
            .contains("discovery for provider `gw` failed: GET "));
        assert_eq!(fs::read(cache_path(&dir, "gw")).expect("cache"), before);
    }

    #[test]
    fn resume_reports_a_changed_or_removed_model_source() {
        let target = ModelTarget::new("gw", "claude");
        let recorded = json!({"kind": "discovered", "endpoint": "/model/info", "fetched_at": "2026-11-04T10:00:00Z", "cache_sha256": "a"});
        let refreshed = json!({"kind": "discovered", "endpoint": "/model/info", "fetched_at": "2026-11-05T10:00:00Z", "cache_sha256": "b"});
        let events = [EventEnvelope::new(
            "session",
            "root",
            None,
            EventKind::SESSION_START,
            euler_event::object([
                ("provider", "gw".into()),
                ("model", "claude".into()),
                ("model_source", recorded.clone()),
            ]),
        )];

        assert_eq!(
            resume_model_source_change(&events, &target, Some(&recorded)),
            None
        );
        assert_eq!(
            resume_model_source_change(&events, &target, Some(&refreshed)).as_deref(),
            Some("note: the model list for gw/claude changed since this session last recorded it; now from /model/info fetched at 2026-11-05T10:00:00Z")
        );
        assert_eq!(
            resume_model_source_change(&events, &target, None).as_deref(),
            Some(
                "note: gw/claude is no longer a discovered model; it is sent as an unlisted model"
            )
        );
    }
}
