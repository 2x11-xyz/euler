use anyhow::{anyhow, Result};
use euler_core::{ContextLimitConfig, EulerHome, SessionConfig, SessionStore};
use euler_provider::catalog::MergedModelCatalog;
use std::path::PathBuf;

pub(crate) const SESSION_ID: &str = "headless-session";
const AGENT_ID: &str = "root";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LiveProvenance {
    HomeSession,
    Explicit(PathBuf),
}

#[derive(Clone, Debug)]
pub(crate) struct HomeSessionRefresh {
    store: SessionStore,
    session_id: String,
}

impl HomeSessionRefresh {
    pub(crate) fn session_store(&self) -> SessionStore {
        self.store.clone()
    }

    pub(crate) fn refresh(&self) -> Result<()> {
        self.store.refresh_session_metadata(&self.session_id)?;
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct LiveSession {
    pub(crate) config: SessionConfig,
    pub(crate) log_path: PathBuf,
    pub(crate) refresh: Option<HomeSessionRefresh>,
}

#[derive(Debug)]
pub(crate) struct ResumeTarget {
    pub(crate) log_path: PathBuf,
    pub(crate) refresh: Option<HomeSessionRefresh>,
    pub(crate) display_label: Option<String>,
    pub(crate) session_name: Option<String>,
}

pub(crate) fn resolve_resume_target(path_or_id: PathBuf) -> Result<ResumeTarget> {
    if path_or_id.is_file() || path_or_id.components().count() > 1 {
        return Ok(ResumeTarget {
            log_path: path_or_id,
            refresh: None,
            display_label: None,
            session_name: None,
        });
    }
    let Some(reference) = path_or_id.to_str() else {
        return Ok(ResumeTarget {
            log_path: path_or_id,
            refresh: None,
            display_label: None,
            session_name: None,
        });
    };
    let home = EulerHome::resolve()?;
    let store = SessionStore::new(home)?;
    let Some(record) = store.resolve_session_reference(reference)? else {
        return Err(anyhow!("no session found with id or name {reference}"));
    };
    let display_label = record
        .name()
        .or_else(|| record.title())
        .unwrap_or_else(|| record.id())
        .to_owned();
    Ok(ResumeTarget {
        log_path: record.events_path().to_path_buf(),
        display_label: Some(display_label),
        session_name: record.name().map(str::to_owned),
        refresh: Some(HomeSessionRefresh {
            store,
            session_id: record.id().to_owned(),
        }),
    })
}

pub(crate) fn live_session_config(
    root: PathBuf,
    provider: String,
    model: String,
    provenance: LiveProvenance,
) -> Result<LiveSession> {
    match provenance {
        LiveProvenance::Explicit(path) => Ok(LiveSession {
            config: session_config(root, provider, model, SESSION_ID.to_owned()),
            log_path: path,
            refresh: None,
        }),
        LiveProvenance::HomeSession => {
            let home = EulerHome::resolve()?;
            let store = SessionStore::new(home)?;
            let record = store.create_session()?;
            let config = session_config(root, provider, model, record.id().to_owned());
            Ok(LiveSession {
                config,
                log_path: record.events_path().to_path_buf(),
                refresh: Some(HomeSessionRefresh {
                    store,
                    session_id: record.id().to_owned(),
                }),
            })
        }
    }
}

/// Load this user's stored credential values into the session's redaction
/// set so tool output can never carry them to the canvas or the ledger
/// (secrets contract; issue #56). Best-effort: a missing/corrupt auth file
/// only means fewer known values — the shape-based layer still applies.
pub(crate) fn seed_secret_redaction<D>(
    session: &mut euler_core::Session<D>,
    auth_file: Option<&std::path::Path>,
) {
    for value in stored_credential_values(auth_file) {
        session.add_redacted_secret(value);
    }
}

fn stored_credential_values(auth_file: Option<&std::path::Path>) -> Vec<String> {
    let storage = match auth_file {
        Some(path) => euler_core::auth_storage::AuthStorage::new(path),
        None => euler_core::auth_storage::AuthStorage::new_default(),
    };
    let Ok(storage) = storage else {
        return Vec::new();
    };
    let mut values = Vec::new();
    for provider in storage.list() {
        match storage.get(&provider) {
            Some(euler_core::auth_storage::Credential::ApiKey { key }) => {
                values.push(key.expose_secret().to_owned());
            }
            Some(euler_core::auth_storage::Credential::OAuth {
                access, refresh, ..
            }) => {
                values.push(access.expose_secret().to_owned());
                values.push(refresh.expose_secret().to_owned());
            }
            None => {}
        }
    }
    values
}

/// Fresh-session project-context startup (ADR 0017, phase 3 exposure).
///
/// Builds ONE startup redactor, seeds it with the environment and every
/// stored credential known at startup, and only then resolves the
/// `--project-context` policy against the acknowledgment store, so candidate
/// bytes are redacted before they can contribute to any digest, event, or
/// diagnostic. The returned bootstrap carries that same redactor into the
/// session. The admission-time budget check runs inside the resolver, before
/// any card or dispatch.
///
/// A preflight problem never silently degrades to a bootstrap-less session:
/// recoverable problems collapse into a disabled bootstrap inside the
/// preflight itself, and the one unrecoverable case (a workspace root that
/// cannot be resolved) fails session start honestly, because a session whose
/// root cannot be resolved cannot enforce any path-keyed rule.
pub(crate) fn startup_project_context(
    config: &SessionConfig,
    auth_file: Option<&std::path::Path>,
    policy: Option<euler_core::ProjectContextPolicy>,
    trusted_local: bool,
) -> Result<euler_core::ProjectContextBootstrap> {
    let redactor = euler_core::redaction::SecretRedactor::from_env();
    for value in stored_credential_values(auth_file) {
        redactor.add_value(value);
    }
    let options = euler_core::ProjectContextResolveOptions {
        policy: policy.unwrap_or(euler_core::ProjectContextPolicy::DEFAULT),
        session_kind: config.session_kind,
        trusted_local,
    };
    let budget = euler_core::AdmissionBudget {
        fixed_instruction_bytes: euler_core::system_instruction_bytes(),
        context_limit_tokens: config.context_limit.map(|limit| limit.limit_tokens()),
        output_reserve_tokens: config
            .max_output_tokens
            .unwrap_or(config.compaction_reserve_tokens as u64),
        canvas_budget_bytes: config.auto_compaction.budget_bytes,
    };
    let resolution = euler_core::ProjectContextBootstrap::resolve(
        &config.root,
        &redactor,
        options,
        config.project_grant_consent_dir.as_deref(),
        budget,
    )
    .map_err(|error| {
        anyhow!("cannot start a session in this folder: {error}; check that the folder exists and is accessible")
    })?;
    finalize_project_context(resolution)
}

/// Turn a policy resolution into the bootstrap the session boots from.
///
/// A budget failure fails honestly before any provider dispatch. An
/// interactive `auto` session that needs an acknowledgment card currently
/// resolves unprompted (runs without the guidance), because the interactive
/// acknowledgment surface lands in a later slice (issue #180); the card will
/// intercept `NeedsAcknowledgment` in the interactive launch path before this
/// finalizer is reached.
fn finalize_project_context(
    resolution: euler_core::ProjectContextResolution,
) -> Result<euler_core::ProjectContextBootstrap> {
    match resolution {
        euler_core::ProjectContextResolution::Resolved(bootstrap) => Ok(*bootstrap),
        euler_core::ProjectContextResolution::Budget(error) => {
            Err(anyhow!("{}", error.user_message()))
        }
        euler_core::ProjectContextResolution::NeedsAcknowledgment(pending) => {
            Ok(pending.unprompted())
        }
    }
}

pub(crate) fn session_config(
    root: PathBuf,
    provider: String,
    model: String,
    session_id: String,
) -> SessionConfig {
    let mut config = SessionConfig::new(root);
    config.session_id = session_id;
    config.agent_id = AGENT_ID.to_owned();
    config.provider = provider;
    config.model = model;
    // Project grants activate only against the user-home consent store, and
    // durable user rules live in the same home; if the home cannot be
    // resolved both stay disabled (fail closed).
    let euler_home = EulerHome::resolve()
        .ok()
        .map(|home| home.root().to_path_buf());
    // User-tier code-swarm reviewer config (data, not authorization): the
    // resolution chain falls back here when the project tier is absent.
    config.code_swarm_user_config_path = EulerHome::resolve()
        .ok()
        .map(|home| home.code_swarm_config_path());
    config.project_grant_consent_dir = euler_home.clone();
    config.user_grant_dir = euler_home;
    config
}

pub(crate) fn apply_catalog_context_limit(
    config: &mut SessionConfig,
    catalog: &MergedModelCatalog,
) {
    if config.context_limit.is_some() {
        return;
    }
    let Some(model) = catalog
        .provider(&config.provider)
        .and_then(|provider| provider.models().find(|model| model.id() == config.model))
    else {
        return;
    };
    let Some(limit_tokens) = model.effective_context_window_tokens() else {
        return;
    };
    config.context_limit =
        ContextLimitConfig::from_catalog_model(limit_tokens, model.auto_compact_token_limit());
}
