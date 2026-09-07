use euler_agents::{AgentBudget, AgentTask};
use euler_core::permissions::ScriptedDecider;
use euler_core::{ContextLimitConfig, ProvenanceWriter, Session, SessionConfig};
use euler_provider::{
    ModelInputItem, ModelProvider, ModelRequest, ModelStreamEvent, ProviderError, ProviderStream,
    StopReason, ToolCall,
};
use euler_sdk::{CancellationToken, Capability};
use std::sync::{Arc, Mutex};

struct Capture {
    calls: Arc<Mutex<Vec<ModelRequest>>>,
    use_tool: bool,
}
impl ModelProvider for Capture {
    fn name(&self) -> &'static str {
        "fixture"
    }
    fn invoke(&self, request: ModelRequest) -> Result<ProviderStream, ProviderError> {
        let mut calls = self.calls.lock().unwrap();
        let count = calls.len();
        calls.push(request);
        let events = if self.use_tool && count == 0 {
            vec![
                Ok(ModelStreamEvent::ToolCall(ToolCall {
                    id: "probe-read".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "note.txt"}),
                })),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::ToolUse,
                    usage: None,
                }),
            ]
        } else {
            vec![
                Ok(ModelStreamEvent::TextDelta("complete".into())),
                Ok(ModelStreamEvent::Finished {
                    stop_reason: StopReason::Completed,
                    usage: None,
                }),
            ]
        };
        Ok(Box::new(events.into_iter()))
    }
}

fn session(
    path: &std::path::Path,
    calls: Arc<Mutex<Vec<ModelRequest>>>,
    use_tool: bool,
    context: Option<u64>,
) -> Session<ScriptedDecider> {
    std::fs::create_dir_all(path).unwrap();
    std::fs::write(path.join("note.txt"), "child-only-note").unwrap();
    let writer = ProvenanceWriter::new(path.join("events.jsonl")).unwrap();
    let mut config = SessionConfig::new(path);
    config.context_limit = context.and_then(|v| ContextLimitConfig::new(v, 1.0));
    config.max_output_tokens = Some(8);
    Session::new(
        config,
        Capture { calls, use_tool },
        ScriptedDecider::new(vec![]),
    )
    .with_provenance(writer)
}

fn main() {
    let fixtures = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(vec![]));
    let mut child = session(&fixtures.path().join("history"), calls.clone(), true, None);
    let task = AgentTask::new_inheriting_target("read note.txt then summarize", "probe")
        .unwrap()
        .with_parent_canvas(false)
        .with_capabilities([Capability::FsRead])
        .with_budget(AgentBudget::new(Some(2), Some(2), None).unwrap());
    let result = child.spawn_companion(task).unwrap();
    let captured = calls.lock().unwrap();
    println!(
        "own-history child_ok={} model_rounds={} second_request_items={} own_tool_outputs={}",
        result.result.ok(),
        captured.len(),
        captured[1].input.len(),
        captured[1]
            .input
            .iter()
            .filter(|i| matches!(i, ModelInputItem::ToolOutput { .. }))
            .count()
    );
    drop(captured);

    let task = AgentTask::new_inheriting_target("summarize", "probe")
        .unwrap()
        .with_parent_canvas(false)
        .with_explicit_context("x".repeat(8192))
        .unwrap()
        .with_budget(AgentBudget::new(Some(1), Some(0), Some(8)).unwrap());
    let calls = Arc::new(Mutex::new(vec![]));
    let mut seq = session(
        &fixtures.path().join("sequential"),
        calls.clone(),
        false,
        Some(10),
    );
    let seq_result = seq.spawn_companion(task.clone()).unwrap();
    println!("sequential-context-limit child_ok={} provider_calls={} context_limit_tokens=10 explicit_context_bytes=8192",
        seq_result.result.ok(), calls.lock().unwrap().len());
    let calls = Arc::new(Mutex::new(vec![]));
    let mut par = session(
        &fixtures.path().join("parallel"),
        calls.clone(),
        false,
        Some(10),
    );
    let par_result = par
        .spawn_reviewers_parallel(vec![task], &CancellationToken::new())
        .unwrap();
    println!(
        "parallel-context-limit child_ok={} provider_calls={} error={:?}",
        par_result[0].result.ok(),
        calls.lock().unwrap().len(),
        par_result[0].result.error()
    );
}
