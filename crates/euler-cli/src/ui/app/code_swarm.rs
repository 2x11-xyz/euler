use super::*;

/// /code-swarm orchestration state. v1 note: reviewer companions run
/// serially through the pending-run queue; counters assume no interleaved
/// manual /companion runs while a swarm is active.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CodeSwarmRun {
    Briefing,
    Reviewing { remaining: usize, total: usize },
    Reporting { total: usize },
}

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
            Ok(_) => self.teach_notice(format!(
                "code-swarm → {count} models · saved as euler default"
            )),
            Err(error) => self.teach_notice(format!(
                "code-swarm → {count} models · session only (save failed: {error})"
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

    pub(super) fn code_swarm_review(
        &mut self,
        prompt: Option<String>,
        personas: Option<Vec<String>>,
    ) -> CoreEffect {
        if self.code_swarm_run.is_some() {
            return self.notice_item("a code-swarm review is already running".to_owned());
        }
        let models = self.code_swarm_effective_models();
        if models.is_empty() {
            return self.notice_item(
                "no reviewer models available — /code-swarm to pick, /login for providers"
                    .to_owned(),
            );
        }
        let mut input = serde_json::Map::new();
        input.insert(
            "models".to_owned(),
            serde_json::Value::Array(
                models
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
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
        self.code_swarm_run = Some(CodeSwarmRun::Briefing);
        self.notice = Some(format!("code-swarm: briefing {} reviewers", models.len()));
        let effect = self.extension_run(
            "code-swarm".to_owned(),
            "review-brief".to_owned(),
            serde_json::Value::Object(input),
            None,
        );
        if self.code_swarm_run.is_some() && matches!(self.state, AppState::Empty) {
            // extension_run refused (no active session); clear the run.
            self.code_swarm_run = None;
        }
        effect
    }

    /// Advance the swarm after review-brief completes: queue one companion
    /// per brief; the pending-run queue drains them serially.
    pub(super) fn code_swarm_on_brief_complete(&mut self, output: &serde_json::Value) {
        let briefs: Vec<serde_json::Value> =
            output["briefs"].as_array().cloned().unwrap_or_default();
        if briefs.is_empty() {
            self.code_swarm_run = None;
            let _ = self.notice_item("code-swarm: review-brief returned no briefs".to_owned());
            return;
        }
        let mut queued = 0usize;
        for brief in &briefs {
            match crate::companion_run::parse_agent_task_value(brief) {
                Ok(task) => {
                    self.pending_runs
                        .push_back(PendingRunRequest::Companion(CompanionRunRequest { task }));
                    queued += 1;
                }
                Err(error) => {
                    let _ =
                        self.notice_item(format!("code-swarm: skipping malformed brief: {error}"));
                }
            }
        }
        if queued == 0 {
            self.code_swarm_run = None;
            return;
        }
        self.code_swarm_run = Some(CodeSwarmRun::Reviewing {
            remaining: queued,
            total: queued,
        });
        self.notice = Some(format!("code-swarm: running reviewer 1/{queued}"));
    }

    /// Advance the swarm after each reviewer companion completes; queue the
    /// consolidation report when the last one lands.
    pub(super) fn code_swarm_on_companion_done(&mut self) {
        let Some(CodeSwarmRun::Reviewing { remaining, total }) = self.code_swarm_run.clone() else {
            return;
        };
        let remaining = remaining.saturating_sub(1);
        if remaining > 0 {
            self.code_swarm_run = Some(CodeSwarmRun::Reviewing { remaining, total });
            self.notice = Some(format!(
                "code-swarm: running reviewer {}/{total}",
                total - remaining + 1
            ));
            return;
        }
        self.code_swarm_run = Some(CodeSwarmRun::Reporting { total });
        match self.resolve_extension_run(
            "code-swarm".to_owned(),
            "review-report".to_owned(),
            serde_json::Value::Object(serde_json::Map::new()),
            None,
        ) {
            Ok(request) => {
                self.pending_runs
                    .push_back(PendingRunRequest::Extension(request));
                self.notice = Some("code-swarm: consolidating report".to_owned());
            }
            Err(error) => {
                self.code_swarm_run = None;
                let _ = self.notice_item(format!("code-swarm: report failed to start: {error}"));
            }
        }
    }

    pub(super) fn code_swarm_on_report_complete(
        &mut self,
        outcome_ok: bool,
        output: Option<&serde_json::Value>,
    ) {
        let Some(CodeSwarmRun::Reporting { total }) = self.code_swarm_run.clone() else {
            return;
        };
        self.code_swarm_run = None;
        if !outcome_ok {
            let _ = self.notice_item(format!(
                "✗ code-swarm review failed during report consolidation ({total} reviewers ran)"
            ));
            return;
        }
        let path = output
            .and_then(|output| output["relative_path"].as_str())
            .unwrap_or("(unknown path)");
        let reviewers = output
            .and_then(|output| output["reviewer_count"].as_u64())
            .unwrap_or(total as u64);
        let _ = self.summary_item(format!(
            "✓ code-swarm review complete · {reviewers} reviewers · artifact {path}"
        ));
    }
}
