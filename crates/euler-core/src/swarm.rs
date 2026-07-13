//! Persisted CodeSwarm reviewer configuration: project and user tiers.
//!
//! Config is **data, not authorization** (multi-agent contract): it names
//! which reviewer targets a swarm would use; spawning still rides the
//! `agent-spawn` permission machinery unchanged. Unlike project permission
//! grants there is no consent-store pairing — a repo-shipped config file
//! grants nothing by itself.
//!
//! Resolution chain (every entry point — TUI `/review`, headless
//! `extension_run`, the `code_swarm_review` tool):
//! explicit models on the invocation → project tier → user tier → honest
//! unconfigured failure. A malformed file at a tier is an error, never a
//! silent fall-through.

use euler_agents::{MAX_MODEL_BYTES, MAX_PERSONA_BYTES, MAX_PROVIDER_BYTES};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Hard cap on configured reviewers (matches the code-swarm extension cap).
pub const MAX_SWARM_REVIEWERS: usize = 5;

/// Canonical remediation-bearing unconfigured error (multi-agent contract):
/// every entry seam — TUI, headless `extension_run`, the `code_swarm_review`
/// tool — emits this text instead of invoking the swarm, and it must name
/// working invocations only. The invocations are pinned by tests; update
/// them together with the surfaces they describe.
pub const UNCONFIGURED_SWARM_ERROR: &str = "code-swarm is not configured: no reviewer models are set for this project or user, and none were passed. Pick reviewers in the TUI with /code-swarm (persists to this project; /code-swarm --user for a global default), or pass explicit one-off targets: TUI `/review --model provider::model`, or headless (`euler run`) control line `extension_run code-swarm.review {\"models\":[\"provider::model\"],\"prompt\":\"explicit review context\"}`.";
/// Max bytes for the config file at either tier.
const MAX_SWARM_CONFIG_BYTES: u64 = 16 * 1024;

const SWARM_CONFIG_FILE: &str = "code-swarm.json";
const EULER_DIR: &str = ".euler";
const SWARM_CONFIG_VERSION: u64 = 1;

/// One configured reviewer: a `provider::model` target plus an optional
/// persona (reviewer-charter) label interpreted by the orchestration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwarmReviewer {
    provider: String,
    model: String,
    persona: Option<String>,
}

impl SwarmReviewer {
    pub fn parse(target: &str, persona: Option<&str>) -> Result<Self, SwarmConfigError> {
        let Some((provider, model)) = target.split_once("::") else {
            return Err(SwarmConfigError::BadTarget {
                target: target.to_owned(),
                reason: "must use provider::model form",
            });
        };
        let provider = provider.trim();
        let model = model.trim();
        if provider.is_empty() || model.is_empty() {
            return Err(SwarmConfigError::BadTarget {
                target: target.to_owned(),
                reason: "must name both provider and model",
            });
        }
        if provider.len() > MAX_PROVIDER_BYTES || model.len() > MAX_MODEL_BYTES {
            return Err(SwarmConfigError::BadTarget {
                target: target.to_owned(),
                reason: "provider or model exceeds the bounded length",
            });
        }
        let persona = persona
            .map(str::trim)
            .filter(|persona| !persona.is_empty())
            .map(str::to_owned);
        if persona
            .as_ref()
            .is_some_and(|persona| persona.len() > MAX_PERSONA_BYTES)
        {
            return Err(SwarmConfigError::BadTarget {
                target: target.to_owned(),
                reason: "persona exceeds the bounded length",
            });
        }
        Ok(Self {
            provider: provider.to_owned(),
            model: model.to_owned(),
            persona,
        })
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn persona(&self) -> Option<&str> {
        self.persona.as_deref()
    }

    /// `provider::model` wire form.
    pub fn target(&self) -> String {
        format!("{}::{}", self.provider, self.model)
    }
}

/// Persisted reviewer set for one tier: 1–5 reviewers, optional default
/// per-reviewer output-token budget.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwarmConfig {
    reviewers: Vec<SwarmReviewer>,
    max_tokens: Option<u64>,
}

impl SwarmConfig {
    pub fn new(
        reviewers: Vec<SwarmReviewer>,
        max_tokens: Option<u64>,
    ) -> Result<Self, SwarmConfigError> {
        if reviewers.is_empty() {
            return Err(SwarmConfigError::NoReviewers);
        }
        if reviewers.len() > MAX_SWARM_REVIEWERS {
            return Err(SwarmConfigError::TooManyReviewers {
                count: reviewers.len(),
            });
        }
        if max_tokens == Some(0) {
            return Err(SwarmConfigError::ZeroMaxTokens);
        }
        // Personas are all-or-none: the orchestration pairs one persona per
        // reviewer, and a ragged list would force it to invent labels for
        // the gaps. Reject at write time instead of failing a later run.
        let labeled = reviewers
            .iter()
            .filter(|reviewer| reviewer.persona.is_some())
            .count();
        if labeled != 0 && labeled != reviewers.len() {
            return Err(SwarmConfigError::MixedPersonas);
        }
        Ok(Self {
            reviewers,
            max_tokens,
        })
    }

    /// Build a config from `provider::model` strings (picker/CLI form).
    pub fn from_targets<S: AsRef<str>>(
        targets: &[S],
        max_tokens: Option<u64>,
    ) -> Result<Self, SwarmConfigError> {
        let reviewers = targets
            .iter()
            .map(|target| SwarmReviewer::parse(target.as_ref(), None))
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(reviewers, max_tokens)
    }

    pub fn reviewers(&self) -> &[SwarmReviewer] {
        &self.reviewers
    }

    pub fn max_tokens(&self) -> Option<u64> {
        self.max_tokens
    }

    /// Reviewer targets in `provider::model` wire form.
    pub fn targets(&self) -> Vec<String> {
        self.reviewers.iter().map(SwarmReviewer::target).collect()
    }

    /// Persona labels for configured reviewers, in order. `None` when no
    /// reviewer names a persona (the orchestration then applies its default
    /// charter rotation). Validation guarantees all-or-none labeling.
    pub fn personas(&self) -> Option<Vec<String>> {
        self.reviewers
            .iter()
            .map(|reviewer| reviewer.persona.clone())
            .collect()
    }
}

/// Which tier a resolved config came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SwarmConfigTier {
    Project,
    User,
}

impl SwarmConfigTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

/// One tier's store: a single JSON file, written atomically.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwarmConfigStore {
    path: PathBuf,
}

impl SwarmConfigStore {
    /// Project tier: `<workspace root>/.euler/code-swarm.json`.
    pub fn for_project_root(root: impl AsRef<Path>) -> Self {
        Self {
            path: root.as_ref().join(EULER_DIR).join(SWARM_CONFIG_FILE),
        }
    }

    /// User tier (or tests): an explicit file path. The user-global file is
    /// `<euler home>/code-swarm.json` — see `EulerHome::code_swarm_config_path`.
    pub fn at_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `Ok(None)` when the tier has no config file. Malformed content is an
    /// error, never treated as unconfigured.
    pub fn load(&self) -> Result<Option<SwarmConfig>, SwarmConfigError> {
        let content = match fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(SwarmConfigError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        if content.len() as u64 > MAX_SWARM_CONFIG_BYTES {
            return Err(SwarmConfigError::TooLarge {
                path: self.path.clone(),
                bytes: content.len() as u64,
            });
        }
        let doc: SwarmConfigFile =
            serde_json::from_str(&content).map_err(|source| SwarmConfigError::Invalid {
                path: self.path.clone(),
                source,
            })?;
        if doc.version != SWARM_CONFIG_VERSION {
            return Err(SwarmConfigError::UnsupportedVersion {
                path: self.path.clone(),
                version: doc.version,
            });
        }
        let reviewers = doc
            .reviewers
            .iter()
            .map(|entry| SwarmReviewer::parse(&entry.target, entry.persona.as_deref()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.at_path(&self.path))?;
        SwarmConfig::new(reviewers, doc.max_tokens)
            .map(Some)
            .map_err(|error| error.at_path(&self.path))
    }

    pub fn save(&self, config: &SwarmConfig) -> Result<(), SwarmConfigError> {
        let dir = self.path.parent().ok_or_else(|| SwarmConfigError::Io {
            path: self.path.clone(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "config path has no parent"),
        })?;
        fs::create_dir_all(dir).map_err(|source| SwarmConfigError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let doc = SwarmConfigFile {
            version: SWARM_CONFIG_VERSION,
            reviewers: config
                .reviewers
                .iter()
                .map(|reviewer| SwarmReviewerEntry {
                    target: reviewer.target(),
                    persona: reviewer.persona.clone(),
                })
                .collect(),
            max_tokens: config.max_tokens,
        };
        let bytes =
            serde_json::to_vec_pretty(&doc).map_err(|source| SwarmConfigError::Serialize {
                path: self.path.clone(),
                source,
            })?;
        write_atomic(&self.path, &bytes)
    }

    /// Remove the tier's config file. Returns whether a config existed.
    pub fn clear(&self) -> Result<bool, SwarmConfigError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(SwarmConfigError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }
}

/// Persisted-tier resolution: project wins, user is the fallback, neither is
/// `Ok(None)` (unconfigured). Explicit invocation models are the caller's
/// step 1 and never reach this function.
pub fn resolve_swarm_config(
    project_root: &Path,
    user_config_path: Option<&Path>,
) -> Result<Option<(SwarmConfig, SwarmConfigTier)>, SwarmConfigError> {
    if let Some(config) = SwarmConfigStore::for_project_root(project_root).load()? {
        return Ok(Some((config, SwarmConfigTier::Project)));
    }
    if let Some(path) = user_config_path {
        if let Some(config) = SwarmConfigStore::at_path(path).load()? {
            return Ok(Some((config, SwarmConfigTier::User)));
        }
    }
    Ok(None)
}

#[derive(Debug, Serialize, Deserialize)]
struct SwarmConfigFile {
    version: u64,
    #[serde(default)]
    reviewers: Vec<SwarmReviewerEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SwarmReviewerEntry {
    target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    persona: Option<String>,
}

#[derive(Debug, Error)]
pub enum SwarmConfigError {
    #[error("swarm config needs at least one reviewer")]
    NoReviewers,
    #[error("swarm config lists {count} reviewers; the cap is {MAX_SWARM_REVIEWERS}")]
    TooManyReviewers { count: usize },
    #[error("swarm config max_tokens must be greater than zero")]
    ZeroMaxTokens,
    #[error("swarm config personas must label every reviewer or none")]
    MixedPersonas,
    #[error("invalid reviewer target `{target}`: {reason}")]
    BadTarget {
        target: String,
        reason: &'static str,
    },
    #[error("swarm config file too large at {}: {bytes} bytes", path.display())]
    TooLarge { path: PathBuf, bytes: u64 },
    #[error("unsupported swarm config version {version} at {}", path.display())]
    UnsupportedVersion { path: PathBuf, version: u64 },
    #[error("invalid swarm config file {}: {source}", path.display())]
    Invalid {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid swarm config file {}: {source}", path.display())]
    InvalidEntry {
        path: PathBuf,
        #[source]
        source: Box<SwarmConfigError>,
    },
    #[error("failed to serialize swarm config at {}: {source}", path.display())]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("swarm config I/O at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl SwarmConfigError {
    /// Attach the file path to a validation error surfaced during `load`.
    fn at_path(self, path: &Path) -> Self {
        match self {
            error @ (Self::TooLarge { .. }
            | Self::UnsupportedVersion { .. }
            | Self::Invalid { .. }
            | Self::InvalidEntry { .. }
            | Self::Serialize { .. }
            | Self::Io { .. }) => error,
            error => Self::InvalidEntry {
                path: path.to_path_buf(),
                source: Box::new(error),
            },
        }
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), SwarmConfigError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let temp_path = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(SWARM_CONFIG_FILE),
        ulid::Ulid::new()
    ));
    let io_error = |source: io::Error, at: &Path| SwarmConfigError::Io {
        path: at.to_path_buf(),
        source,
    };
    {
        use std::io::Write as _;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|source| io_error(source, &temp_path))?;
        file.write_all(bytes)
            .map_err(|source| io_error(source, &temp_path))?;
        file.flush()
            .map_err(|source| io_error(source, &temp_path))?;
        file.sync_data()
            .map_err(|source| io_error(source, &temp_path))?;
    }
    fs::rename(&temp_path, path).map_err(|source| io_error(source, path))?;
    #[cfg(unix)]
    {
        let _ = fs::File::open(dir).and_then(|f| f.sync_all());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_store_round_trips_targets_personas_and_max_tokens() {
        let temp = tempfile::tempdir().expect("temp");
        let store = SwarmConfigStore::for_project_root(temp.path());
        assert_eq!(store.load().expect("empty load"), None);

        let config = SwarmConfig::new(
            vec![
                SwarmReviewer::parse("anthropic::claude-opus-5", Some("correctness"))
                    .expect("reviewer"),
                SwarmReviewer::parse("openai::gpt-5.5", Some("safety")).expect("reviewer"),
            ],
            Some(4096),
        )
        .expect("config");
        store.save(&config).expect("save");

        let loaded = store.load().expect("load").expect("configured");
        assert_eq!(loaded, config);
        assert_eq!(
            loaded.targets(),
            vec!["anthropic::claude-opus-5", "openai::gpt-5.5"]
        );
        assert_eq!(loaded.max_tokens(), Some(4096));
        assert_eq!(loaded.reviewers()[0].persona(), Some("correctness"));
    }

    #[test]
    fn user_store_round_trips_at_explicit_path() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("home").join("code-swarm.json");
        let store = SwarmConfigStore::at_path(&path);
        let config = SwarmConfig::from_targets(&["openrouter::z-ai/glm-5.2"], None).expect("cfg");
        store.save(&config).expect("save");
        assert_eq!(store.load().expect("load"), Some(config));
        assert!(store.clear().expect("clear"));
        assert_eq!(store.load().expect("load after clear"), None);
        assert!(!store.clear().expect("second clear is a no-op"));
    }

    #[test]
    fn resolution_prefers_project_then_user_then_none() {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("root");
        let user_path = temp.path().join("home").join("code-swarm.json");

        assert_eq!(
            resolve_swarm_config(&root, Some(&user_path)).expect("resolve"),
            None
        );

        let user = SwarmConfig::from_targets(&["openai::gpt-5.5"], None).expect("user cfg");
        SwarmConfigStore::at_path(&user_path)
            .save(&user)
            .expect("save user");
        let (resolved, tier) = resolve_swarm_config(&root, Some(&user_path))
            .expect("resolve")
            .expect("user tier");
        assert_eq!(tier, SwarmConfigTier::User);
        assert_eq!(resolved, user);

        let project =
            SwarmConfig::from_targets(&["anthropic::claude-opus-5"], Some(1234)).expect("proj");
        SwarmConfigStore::for_project_root(&root)
            .save(&project)
            .expect("save project");
        let (resolved, tier) = resolve_swarm_config(&root, Some(&user_path))
            .expect("resolve")
            .expect("project tier");
        assert_eq!(tier, SwarmConfigTier::Project);
        assert_eq!(resolved, project);
    }

    #[test]
    fn malformed_project_config_is_an_error_not_a_fallthrough() {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path();
        let store = SwarmConfigStore::for_project_root(root);
        std::fs::create_dir_all(store.path().parent().expect("dir")).expect("mkdir");
        std::fs::write(store.path(), "{not json").expect("write");
        let user_path = temp.path().join("code-swarm.json");
        SwarmConfigStore::at_path(&user_path)
            .save(&SwarmConfig::from_targets(&["openai::gpt-5.5"], None).expect("cfg"))
            .expect("save user");

        assert!(resolve_swarm_config(root, Some(&user_path)).is_err());
    }

    #[test]
    fn config_validation_rejects_bad_shapes() {
        assert!(matches!(
            SwarmConfig::from_targets::<&str>(&[], None),
            Err(SwarmConfigError::NoReviewers)
        ));
        assert!(matches!(
            SwarmConfig::from_targets(&["a::b"; 6], None),
            Err(SwarmConfigError::TooManyReviewers { count: 6 })
        ));
        assert!(matches!(
            SwarmConfig::from_targets(&["a::b"], Some(0)),
            Err(SwarmConfigError::ZeroMaxTokens)
        ));
        for target in ["no-separator", "::model", "provider::", " :: "] {
            assert!(
                SwarmReviewer::parse(target, None).is_err(),
                "target `{target}` must be rejected"
            );
        }
        assert!(SwarmReviewer::parse("a::b", Some(&"x".repeat(200))).is_err());
    }

    #[test]
    fn unsupported_version_is_an_error() {
        let temp = tempfile::tempdir().expect("temp");
        let store = SwarmConfigStore::for_project_root(temp.path());
        std::fs::create_dir_all(store.path().parent().expect("dir")).expect("mkdir");
        std::fs::write(
            store.path(),
            r#"{"version": 2, "reviewers": [{"target": "a::b"}]}"#,
        )
        .expect("write");
        assert!(matches!(
            store.load(),
            Err(SwarmConfigError::UnsupportedVersion { version: 2, .. })
        ));
    }

    #[test]
    fn personas_are_all_or_none() {
        assert!(matches!(
            SwarmConfig::new(
                vec![
                    SwarmReviewer::parse("a::b", Some("correctness")).expect("r"),
                    SwarmReviewer::parse("c::d", None).expect("r"),
                ],
                None,
            ),
            Err(SwarmConfigError::MixedPersonas)
        ));
        let labeled = SwarmConfig::new(
            vec![
                SwarmReviewer::parse("a::b", Some("correctness")).expect("r"),
                SwarmReviewer::parse("c::d", Some("safety")).expect("r"),
            ],
            None,
        )
        .expect("config");
        assert_eq!(
            labeled.personas(),
            Some(vec!["correctness".to_owned(), "safety".to_owned()])
        );
        let unlabeled = SwarmConfig::from_targets(&["a::b"], None).expect("config");
        assert_eq!(unlabeled.personas(), None);
    }
}
