use euler_core::{
    assemble_canvas, read_provenance, AutoCompactionPolicy, ProvenanceWriter, Session,
    SessionConfig,
};
use euler_provider::{FixtureResponse, ScriptedProvider, ToolCall};
use serde_json::json;
use std::fs;
use std::time::Instant;

const WORKLOAD: &str = "20_turn_20_large_read";
const TURNS: usize = 20;
const LARGE_BYTES_PER_TOOL: usize = 10_000;

#[test]
#[ignore = "measurement harness; run explicitly with --ignored --nocapture"]
fn measure_replay_cost_for_named_scripted_workload() {
    let temp = tempfile::tempdir().expect("temp dir");
    for index in 0..TURNS {
        let body = format!("{index:02}:{}\n", "x".repeat(LARGE_BYTES_PER_TOOL - 4));
        fs::write(temp.path().join(format!("large-{index:02}.txt")), body).expect("fixture file");
    }

    let mut responses = Vec::new();
    for index in 0..TURNS {
        responses.push(FixtureResponse::ToolCalls(vec![ToolCall {
            id: format!("call-large-{index:02}"),
            name: "read_file".to_owned(),
            input: json!({"path": format!("large-{index:02}.txt"), "max_bytes": 12000}),
        }]));
        responses.push(FixtureResponse::Assistant(format!(
            "recorded large read {index:02}"
        )));
    }

    let log = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(&log).expect("writer");
    let provider = ScriptedProvider::new(responses);
    let mut session = Session::new(SessionConfig::new(temp.path()), provider, DenyDecider)
        .with_provenance(writer);
    for index in 0..TURNS {
        session
            .run_turn(&format!("read large file {index:02}"))
            .expect("turn");
    }
    drop(session);

    let jsonl_bytes = fs::metadata(&log).expect("log metadata").len();
    let blob_bytes = dir_bytes(&temp.path().join("blobs"));
    let event_count = fs::read_to_string(&log)
        .expect("read log")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();

    let started = Instant::now();
    let replayed = read_provenance(&log).expect("replay");
    let canvas_items = assemble_canvas(&replayed, &AutoCompactionPolicy::default()).len();
    let replay_ms = started.elapsed().as_secs_f64() * 1000.0;

    println!("workload={WORKLOAD}");
    println!("turns={TURNS}");
    println!("tool_calls={TURNS}");
    println!("large_output_bytes_each={LARGE_BYTES_PER_TOOL}");
    println!("event_count={event_count}");
    println!("jsonl_bytes={jsonl_bytes}");
    println!("blob_bytes={blob_bytes}");
    println!("replay_wall_clock_ms={replay_ms:.3}");
    println!("canvas_items_after_replay={canvas_items}");
}

fn dir_bytes(path: &std::path::Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum()
}

struct DenyDecider;

impl euler_core::permissions::PermissionDecider for DenyDecider {
    fn decide(
        &mut self,
        _request: &euler_core::permissions::PermissionRequest,
    ) -> euler_core::permissions::DeciderVerdict {
        euler_core::permissions::DeciderVerdict::Deny
    }
}

/// Scaling audit: how does one canvas assembly cost grow with event-log
/// length, and what is the implied whole-session (per-round re-assembly)
/// cost? Run with:
///   EULER_COST_LOG=/path/to/events.jsonl cargo test -p euler-core \
///     --test replay_cost measure_canvas_assembly_scaling -- --ignored --nocapture
#[test]
#[ignore = "measurement harness; run explicitly with --ignored --nocapture"]
fn measure_canvas_assembly_scaling() {
    let Some(path) = std::env::var_os("EULER_COST_LOG") else {
        eprintln!("EULER_COST_LOG not set; skipping");
        return;
    };
    let events = read_provenance(std::path::Path::new(&path)).expect("replay log");
    let total = events.len();
    println!("log={} events={total}", path.to_string_lossy());
    let mut checkpoints: Vec<usize> = (1..=10).map(|i| total * i / 10).collect();
    checkpoints.dedup();
    let mut cumulative_ms = 0.0;
    for &n in &checkpoints {
        let slice = &events[..n];
        // Median of 5 to steady the timing.
        let mut samples = Vec::new();
        let mut items = 0;
        for _ in 0..5 {
            let started = Instant::now();
            items = assemble_canvas(slice, &AutoCompactionPolicy::default()).len();
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = samples[2];
        cumulative_ms += median;
        println!("prefix_events={n} canvas_items={items} assemble_ms={median:.3}");
    }
    // Whole-session estimate: one assembly per model round over a growing log.
    let model_rounds = events
        .iter()
        .filter(|event| event.kind.as_str() == euler_event::EventKind::MODEL_CALL)
        .count();
    println!("model_rounds={model_rounds}");
    println!("sampled_cumulative_ms={cumulative_ms:.1} (10 checkpoint assemblies)");
}
