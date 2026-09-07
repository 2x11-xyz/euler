use euler_agents::{AgentBudget, AgentTask};
use euler_core::permissions::ScriptedDecider;
use euler_core::redaction::SecretRedactor;
use euler_core::{
    AdmissionBudget, ProjectContextBootstrap, ProjectContextPolicy, ProjectContextResolution,
    ProjectContextResolveOptions, ProvenanceWriter, Session, SessionConfig, SessionKind,
};
use euler_provider::{
    ModelInputItem, ModelProvider, ModelRequest, ModelStreamEvent, ProviderError, ProviderStream,
    StopReason,
};
use std::sync::{Arc, Mutex};

const MARKER: &str = "PROJECT_ONLY_COMPILATION_RULE_9387";
struct Capture {
    calls: Arc<Mutex<Vec<ModelRequest>>>,
}
impl ModelProvider for Capture {
    fn name(&self) -> &'static str {
        "fixture"
    }
    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        let is_shadow = request.instructions.contains("shadow compactor");
        let mut calls = self.calls.lock().unwrap();
        let content = if is_shadow {
            let includes_marker = request.prompt_text().contains(MARKER);
            serde_json::json!({"goal":"continue", "plan":"short", "compiler_state":"", "modified_files":[], "decisions":if includes_marker {vec![MARKER]} else {vec![]}, "working_set":[]}).to_string()
        } else {
            "complete".to_owned()
        };
        calls.push(request);
        Ok(Box::new(
            vec![
                Ok(ModelStreamEvent::TextDelta(content)),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None,
                }),
            ]
            .into_iter(),
        ))
    }
}
fn main() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("EULER.md"), format!("Rule: {MARKER}\n")).unwrap();
    let bootstrap = match ProjectContextBootstrap::resolve(
        root,
        &SecretRedactor::default(),
        ProjectContextResolveOptions {
            policy: ProjectContextPolicy::On,
            session_kind: SessionKind::Interactive,
            trusted_local: false,
        },
        None,
        AdmissionBudget {
            fixed_instruction_bytes: 0,
            context_limit_tokens: None,
            output_reserve_tokens: 0,
            canvas_budget_bytes: 1024 * 1024,
        },
    )
    .unwrap()
    {
        ProjectContextResolution::Resolved(v) => *v,
        _ => panic!("unresolved"),
    };
    let calls = Arc::new(Mutex::new(vec![]));
    let mut config = SessionConfig::new(root);
    config.project_context = Some(bootstrap);
    let mut session = Session::new(
        config,
        Capture {
            calls: calls.clone(),
        },
        ScriptedDecider::new(vec![]),
    )
    .with_provenance(ProvenanceWriter::new(root.join("events.jsonl")).unwrap());
    session
        .run_turn(&"ordinary context for compaction ".repeat(1000))
        .unwrap();
    let status = session.compact_and_wait().unwrap();
    let task = AgentTask::new_inheriting_target("review the work", "probe")
        .unwrap()
        .with_budget(AgentBudget::new(Some(1), Some(0), None).unwrap());
    let result = session.spawn_companion(task).unwrap();
    let calls = calls.lock().unwrap();
    let child = calls.last().unwrap();
    println!("compaction_status={status:?} child_ok={} total_requests={} child_typed_project_items={} child_prompt_contains_project_only_marker={}", result.result.ok(), calls.len(), child.input.iter().filter(|v|matches!(v,ModelInputItem::ProjectContext{..})).count(), child.prompt_text().contains(MARKER));
}
