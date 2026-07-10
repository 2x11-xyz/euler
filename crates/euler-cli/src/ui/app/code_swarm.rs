use super::*;

pub(super) fn load_code_swarm_models_startup() -> Vec<String> {
    let Some(path) = crate::model_preference::default_model_preference_path() else {
        return Vec::new();
    };
    match crate::model_preference::load_code_swarm_models_preference(&path) {
        crate::model_preference::CodeSwarmModelsLoad::Loaded(models) => models,
        _ => Vec::new(),
    }
}

impl AppCore {
    pub(super) fn code_swarm_save_models(&mut self, models: Vec<String>) -> CoreEffect {
        let count = models.len();
        self.code_swarm_models = models;
        let persisted = crate::model_preference::default_model_preference_path()
            .map(|path| {
                crate::model_preference::save_code_swarm_models_preference(
                    &path,
                    &self.code_swarm_models,
                )
            })
            .transpose();
        self.rebuild_bottom_surface();
        match persisted {
            Ok(_) => self.summary_item(format!(
                "✓ code-swarm → {count} models · saved as euler default"
            )),
            Err(error) => self.summary_item(format!(
                "✓ code-swarm → {count} models · session only (save failed: {error})"
            )),
        }
    }

    /// Reviewer model set for a swarm run: saved selection, else the first
    /// three catalog entries (the picker's default of 3).
    fn code_swarm_effective_models(&self) -> Vec<String> {
        if !self.code_swarm_models.is_empty() {
            return self.code_swarm_models.clone();
        }
        self.bottom
            .context()
            .model_choices
            .iter()
            .take(3)
            .map(|choice| format!("{}::{}", choice.provider, choice.model))
            .collect()
    }

    /// `/code-swarm review` — one extension command; the extension
    /// self-orchestrates its reviewers through `HostApi::spawn_agent`.
    pub(super) fn code_swarm_review(
        &mut self,
        prompt: Option<String>,
        personas: Option<Vec<String>>,
    ) -> CoreEffect {
        let models = self.code_swarm_effective_models();
        if models.is_empty() {
            return self.notice_item(
                "no reviewer models available — /code-swarm to pick, /login for providers"
                    .to_owned(),
            );
        }
        self.notice = Some(format!(
            "code-swarm: reviewing with {} agents",
            models.len()
        ));
        self.extension_run(
            "code-swarm".to_owned(),
            "review".to_owned(),
            code_swarm_review_input(models, prompt, personas),
            None,
        )
    }
}

pub(super) fn code_swarm_review_input(
    models: Vec<String>,
    prompt: Option<String>,
    personas: Option<Vec<String>>,
) -> serde_json::Value {
    let mut input = serde_json::Map::new();
    input.insert(
        "models".to_owned(),
        serde_json::Value::Array(models.into_iter().map(serde_json::Value::String).collect()),
    );
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
