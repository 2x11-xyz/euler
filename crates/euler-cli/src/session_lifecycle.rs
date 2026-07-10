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
}

pub(crate) fn resolve_resume_target(path_or_id: PathBuf) -> Result<ResumeTarget> {
    if path_or_id.is_file() || path_or_id.components().count() > 1 {
        return Ok(ResumeTarget {
            log_path: path_or_id,
            refresh: None,
        });
    }
    let Some(reference) = path_or_id.to_str() else {
        return Ok(ResumeTarget {
            log_path: path_or_id,
            refresh: None,
        });
    };
    let home = EulerHome::resolve()?;
    let store = SessionStore::new(home)?;
    let Some(record) = store.resolve_session_reference(reference)? else {
        return Err(anyhow!("no session found with id or name {reference}"));
    };
    Ok(ResumeTarget {
        log_path: record.events_path().to_path_buf(),
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
    // Project grants activate only against the user-home consent store; if
    // the home cannot be resolved they stay disabled (fail closed).
    config.project_grant_consent_dir = EulerHome::resolve()
        .ok()
        .map(|home| home.root().to_path_buf());
    config
}

pub(crate) fn apply_catalog_context_limit(
    config: &mut SessionConfig,
    catalog: &MergedModelCatalog,
) {
    if config.context_limit.is_some() {
        return;
    }
    let Some(limit_tokens) = catalog
        .provider(&config.provider)
        .and_then(|provider| provider.models().find(|model| model.id() == config.model))
        .and_then(|model| model.context_window_tokens())
    else {
        return;
    };
    config.context_limit = ContextLimitConfig::from_catalog_window(limit_tokens);
}
