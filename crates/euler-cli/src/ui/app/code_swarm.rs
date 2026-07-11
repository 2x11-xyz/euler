use super::*;
use crate::code_swarm_config;
use euler_core::{SwarmConfig, SwarmConfigStore};

/// Startup view of the persisted reviewer set for picker preselection and
/// the palette context: the contract chain's persisted tiers (project, then
/// user). Read errors surface at run time, not at startup.
pub(super) fn load_code_swarm_models_startup() -> Vec<String> {
    code_swarm_config::resolved_targets_for_display(&code_swarm_config::workspace_root())
        .map(|(targets, _tier)| targets)
        .unwrap_or_default()
}

impl AppCore {
    /// Persist the picker selection to one tier of the swarm config store
    /// (project by default; `--user` for the user-global tier).
    pub(super) fn code_swarm_save_models(
        &mut self,
        models: Vec<String>,
        user_tier: bool,
    ) -> CoreEffect {
        let count = models.len();
        let config = match SwarmConfig::from_targets(&models, None) {
            Ok(config) => config,
            Err(error) => return self.notice_item(format!("code-swarm save failed: {error}")),
        };
        let (store, tier_label) = match self.code_swarm_store(user_tier) {
            Ok(store) => store,
            Err(message) => return self.notice_item(message),
        };
        // Cache feeds the picker preselection and the palette context; the
        // run path re-reads the stores so external edits still win.
        self.code_swarm_models = models;
        self.rebuild_bottom_surface();
        // Spec v2.1 §5c: dim provenance line, not a summary — this is a
        // config confirmation, not a result.
        match store.save(&config) {
            Ok(()) => self.teach_notice(format!(
                "✓ code-swarm · {count} reviewers configured · {tier_label} tier"
            )),
            Err(error) => self.notice_item(format!("code-swarm save failed: {error}")),
        }
    }

    /// `/code-swarm clear [--user]` — remove one tier's persisted config.
    pub(super) fn code_swarm_clear(&mut self, user_tier: bool) -> CoreEffect {
        let (store, tier_label) = match self.code_swarm_store(user_tier) {
            Ok(store) => store,
            Err(message) => return self.notice_item(message),
        };
        let outcome = store.clear();
        self.code_swarm_models = load_code_swarm_models_startup();
        self.rebuild_bottom_surface();
        match outcome {
            Ok(true) => self.teach_notice(format!("✓ code-swarm · {tier_label} tier cleared")),
            Ok(false) => {
                self.teach_notice(format!("code-swarm · no {tier_label}-tier config to clear"))
            }
            Err(error) => self.notice_item(format!("code-swarm clear failed: {error}")),
        }
    }

    fn code_swarm_store(
        &self,
        user_tier: bool,
    ) -> Result<(SwarmConfigStore, &'static str), String> {
        if user_tier {
            let path = code_swarm_config::user_config_path().ok_or_else(|| {
                "code-swarm user tier unavailable: the euler home cannot be resolved (set HOME \
                 or EULER_HOME)"
                    .to_owned()
            })?;
            Ok((SwarmConfigStore::at_path(path), "user"))
        } else {
            Ok((
                SwarmConfigStore::for_project_root(code_swarm_config::workspace_root()),
                "project",
            ))
        }
    }

    /// `/code-swarm review` — one extension command; the extension
    /// self-orchestrates its reviewers through `HostApi::spawn_agents`.
    /// Reviewer targets ride the shared resolution chain inside
    /// `extension_run`'s code-swarm seam; this path never guesses models.
    pub(super) fn code_swarm_review(
        &mut self,
        prompt: Option<String>,
        personas: Option<Vec<String>>,
    ) -> CoreEffect {
        self.extension_run(
            "code-swarm".to_owned(),
            "review".to_owned(),
            code_swarm_review_input(prompt, personas),
            None,
        )
    }
}

pub(super) fn code_swarm_review_input(
    prompt: Option<String>,
    personas: Option<Vec<String>>,
) -> serde_json::Value {
    let mut input = serde_json::Map::new();
    if let Some(prompt) = prompt.filter(|prompt| !prompt.trim().is_empty()) {
        input.insert("prompt".to_owned(), serde_json::Value::String(prompt));
    }
    if let Some(personas) = personas.filter(|personas| !personas.is_empty()) {
        input.insert(
            "reviewers".to_owned(),
            serde_json::Value::Array(
                personas
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
    }
    serde_json::Value::Object(input)
}
