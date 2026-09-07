use euler_core::{
    assemble_canvas, capture_workspace_snapshot, query_provenance, AutoCompactionPolicy,
    ProvenanceQuery, ToolRegistry,
};
use euler_event::{object, EventEnvelope};
use serde_json::{json, Value};
use std::{
    fs,
    hint::black_box,
    io::{BufWriter, Write},
    time::Instant,
};

fn event(kind: &str, payload: Value) -> EventEnvelope {
    EventEnvelope::new(
        "audit",
        "root",
        None,
        kind,
        payload.as_object().unwrap().clone(),
    )
}
fn main() {
    println!(
        "Synthetic local timings; relative scaling, not production SLOs; debug_assertions={}",
        cfg!(debug_assertions)
    );
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("events.jsonl");
    for n in [1000, 5000, 10000, 20000] {
        let mut writer = BufWriter::new(fs::File::create(&log).unwrap());
        for i in 0..n {
            let e = event("user.message", json!({"content": format!("message-{i}")}));
            writeln!(writer, "{}", e.to_json_line().unwrap()).unwrap();
        }
        writer.flush().unwrap();
        let mut samples = Vec::new();
        let mut pages = 0;
        for _ in 0..3 {
            let start = Instant::now();
            let mut cursor = None;
            let mut count = 0;
            pages = 0;
            loop {
                let mut q = ProvenanceQuery::new(256);
                q.after_event_id = cursor;
                let p = query_provenance(&log, q).unwrap();
                count += p.events.len();
                pages += 1;
                if !p.truncated {
                    break;
                }
                cursor = p.next_after_event_id;
            }
            assert_eq!(count, n);
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "pagination events={n} pages={pages} median_ms={:.3}",
            samples[1]
        );
    }
    for rounds in [100, 500, 1000] {
        let mut events = vec![event("user.message", json!({"content":"test"}))];
        let mut chunk = Vec::new();
        for i in 0..rounds {
            let call_id = format!("call-{i}");
            let mc = event(
                "model.call",
                json!({"provider":"fixture","model":"fixture"}),
            );
            let mut mr = event(
                "model.result",
                json!({"content":"", "tool_calls":[{"id":call_id,"name":"read_file","input":{"path":"x"}}]}),
            );
            mr.parent = Some(mc.id.clone());
            let mut tc = event(
                "tool.call",
                json!({"id":call_id,"name":"read_file","input":{"path":"x"}}),
            );
            tc.parent = Some(mr.id.clone());
            let mut tr = event(
                "tool.result",
                json!({"id":call_id,"name":"read_file","ok":true,"output":"x".repeat(1000)}),
            );
            tr.parent = Some(tc.id.clone());
            chunk.push(tr.id.clone());
            events.extend([mc, mr, tc, tr]);
            if i % 10 == 9 {
                events.push(event("canvas.swap", json!({
                    "snapshot_start_id":events[0].id,"snapshot_end_id":events[0].id,"frontier_start_id":events[0].id,
                    "policy_version":"1","projection_schema_version":"1","validation_result":"layer1-pass",
                    "projection_blob":"","layer1_compacted_event_ids":chunk
                })));
                chunk.clear();
            }
        }
        let mut samples = Vec::new();
        for _ in 0..3 {
            let start = Instant::now();
            black_box(assemble_canvas(&events, &AutoCompactionPolicy::default()));
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "canvas rounds={rounds} events={} swaps={} median_ms={:.3}",
            events.len(),
            rounds / 10,
            samples[1]
        );
    }
    // Snapshot cost and loss of all evidence at the global file-count ceiling.
    let root = temp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let content = "x".repeat(16 * 1024);
    for i in 0..1000 {
        fs::write(root.join(format!("f-{i:05}.txt")), &content).unwrap();
    }
    let start = Instant::now();
    let before = capture_workspace_snapshot(&root).unwrap();
    let one_ms = start.elapsed().as_secs_f64() * 1000.0;
    fs::write(root.join("f-00000.txt"), "changed").unwrap();
    let after = capture_workspace_snapshot(&root).unwrap();
    assert_eq!(before.changes_to(&after).len(), 1);
    println!("snapshot files=1000 bytes_per_file=16384 one_capture_ms={one_ms:.3}");
    for i in 1000..4097 {
        fs::write(root.join(format!("f-{i:05}.txt")), "x").unwrap();
    }
    let before = capture_workspace_snapshot(&root).unwrap();
    fs::write(root.join("f-00000.txt"), "changed again").unwrap();
    let after = capture_workspace_snapshot(&root).unwrap();
    println!(
        "snapshot files=4097 actual_changes=1 reported_changes={}",
        before.changes_to(&after).len()
    );
    // Built-in read_file reads the whole file and builds a full line vector, despite tiny returned window.
    let path = temp.path().join("large.txt");
    let large = "123456789012345\n".repeat(2 * 1024 * 1024);
    fs::write(&path, &large).unwrap();
    let tools = ToolRegistry::new(temp.path());
    let start = Instant::now();
    let output = tools
        .execute(
            "read_file",
            &json!({"path":"large.txt","max_lines":1,"max_bytes":16}),
        )
        .unwrap();
    println!(
        "read_file file_bytes={} returned_bytes={} elapsed_ms={:.3}",
        large.len(),
        output.output.len(),
        start.elapsed().as_secs_f64() * 1000.0
    );
    black_box(object([] as [(&str, Value); 0]));
}
