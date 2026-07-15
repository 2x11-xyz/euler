#![cfg(unix)]
#![allow(clippy::too_many_lines)]

use euler_managed_process::{
    ManagedProcessExtension, ManagedProcessLimits, ManagedProcessRuntimeError,
};
use euler_sdk::{
    AgentOutcome, ArtifactRecord, ArtifactWrite, Capability, CommandContext, CommandRegistrar,
    DiagnosticsPage, DiagnosticsQuery, EventFeedCheckpoint, Extension, ExtensionCommand,
    ExtensionError, HostAgentRecord, HostAgentResult, HostAgentTask, HostApi,
    ManagedProcessEntrypoint, ProvenancePage, ProvenanceQuery, SpawnAgentTask,
    StaticCommandDescriptor, StaticExtensionDescriptor,
};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[test]
fn python_sdk_round_trips_every_current_host_api() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        &sdk_script(
            r#"
def exercise(context):
    context.host.progress("starting", 0.1)
    page = context.host.query_provenance(limit=2, scan_limit=4)
    diagnostics = context.host.read_diagnostics(tail_lines=3, max_bytes=128)
    state_dir = context.host.state_dir()
    artifact = context.host.write_artifact(
        display_name="result.txt",
        media_type="text/plain",
        data=b"python artifact",
        source_event_ids=[],
        metadata={"from": "python"},
    )
    before = context.host.load_checkpoint("cursor")
    context.host.store_checkpoint("cursor", {"schema_version": 1, "after_event_id": "event-1"})
    record = context.host.record_agent_task_result(
        {"task": "record", "persona": "observer", "provider": "", "model": "", "capabilities": [], "budget": {}, "result_schema": None},
        {"ok": True, "summary": "done", "output": "body", "error": None},
    )
    slot = context.host.update_context_slot("current", "bounded context")
    spawned = context.host.spawn_agent({"task": "one", "persona": "", "provider": "", "model": "", "system_prompt": "", "explicit_context": None, "include_parent_canvas": False, "capabilities": [], "max_turns": None, "max_tool_calls": None, "max_tokens": None})
    spawned_many = context.host.spawn_agents([{"task": "many", "persona": "", "provider": "", "model": "", "system_prompt": "", "explicit_context": None, "include_parent_canvas": False, "capabilities": [], "max_turns": None, "max_tool_calls": None, "max_tokens": None}])
    context.host.progress("finished", 1.0)
    return {
        "events": len(page["events"]),
        "diagnostics": diagnostics["lines"],
        "state_dir": state_dir,
        "artifact": artifact["relative_path"],
        "checkpoint_before": before,
        "record": record["child_agent_id"],
        "slot": slot,
        "spawned": spawned["child_agent_id"],
        "spawned_many": [outcome["child_agent_id"] for outcome in spawned_many],
    }

serve({"exercise": exercise})
"#,
        ),
    );

    let extension = extension(temp.path(), "exercise", all_capabilities());
    let host = FakeHost::default();
    let output = execute(&extension, "exercise", &host).expect("python result");

    assert_eq!(output["events"], json!(0));
    assert_eq!(output["diagnostics"], json!(["diagnostic line"]));
    assert_eq!(
        output["artifact"],
        json!("extensions/python-proof/artifacts/result")
    );
    assert_eq!(output["checkpoint_before"], Value::Null);
    assert_eq!(output["record"], json!("child-record"));
    assert_eq!(output["spawned"], json!("child-live"));
    assert_eq!(output["spawned_many"], json!(["child-live-many"]));

    let artifacts = host.artifacts.borrow();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].display_name, "result.txt");
    assert_eq!(artifacts[0].bytes, b"python artifact");
    assert_eq!(
        artifacts[0].metadata,
        json!({"from": "python"}).as_object().unwrap().clone()
    );
    drop(artifacts);
    assert_eq!(
        *host.checkpoint.borrow(),
        Some(EventFeedCheckpoint {
            schema_version: 1,
            after_event_id: "event-1".to_owned(),
        })
    );
    assert_eq!(
        host.slots.borrow().as_slice(),
        [("current".to_owned(), "bounded context".to_owned())]
    );
    assert_eq!(*host.recorded_agent_tasks.borrow(), 1);
    assert_eq!(*host.spawned_agents.borrow(), 1);
    assert_eq!(*host.spawned_batches.borrow(), 1);
}

#[test]
fn invalid_stdout_and_stderr_secret_never_become_extension_error_text() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        "import sys\nsys.stderr.write('PROCESS_SECRET_DO_NOT_RECORD\\n')\nsys.stdout.write('not json\\n')\nsys.stdout.flush()\n",
    );
    let extension = extension(temp.path(), "exercise", Vec::new());
    let error = execute(&extension, "exercise", &FakeHost::default()).expect_err("protocol error");
    let message = error.to_string();
    assert!(message.contains("managed-process"), "message: {message}");
    assert!(
        !message.contains("PROCESS_SECRET_DO_NOT_RECORD"),
        "message: {message}"
    );
}

#[test]
fn incompatible_handshake_version_fails_before_a_command_can_run() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        r#"import json
import sys

initialize = json.loads(sys.stdin.buffer.readline())
sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocol_version": "not-euler/1"}}) + "\n")
sys.stdout.flush()
"#,
    );
    let error = execute(
        &extension(temp.path(), "exercise", Vec::new()),
        "exercise",
        &FakeHost::default(),
    )
    .expect_err("incompatible protocol version");

    assert_eq!(
        error.to_string(),
        ManagedProcessRuntimeError::ProtocolViolation.to_string()
    );
}

#[test]
fn host_apis_are_unavailable_outside_the_active_command_window() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        r#"import json
import sys

json.loads(sys.stdin.buffer.readline())
sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": "too-early", "method": "euler/host/write-artifact", "params": {"display_name": "bad.txt", "media_type": "text/plain", "bytes_base64": "YmFk"}}) + "\n")
sys.stdout.flush()
"#,
    );
    let host = FakeHost::default();
    let error = execute(
        &extension(
            temp.path(),
            "exercise",
            vec![Capability::ArtifactWrite.as_str().to_owned()],
        ),
        "exercise",
        &host,
    )
    .expect_err("host API before command");

    assert_eq!(
        error.to_string(),
        ManagedProcessRuntimeError::ProtocolViolation.to_string()
    );
    assert!(host.artifacts.borrow().is_empty());
}

#[test]
fn process_error_bodies_are_not_forwarded_to_extension_errors() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        r#"import json
import sys

def read():
    line = sys.stdin.buffer.readline()
    if not line:
        raise SystemExit(0)
    return json.loads(line)

def write(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

initialize = read()
write({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocol_version": "euler-managed-process/1"}})
read()
command = read()
write({"jsonrpc": "2.0", "id": command["id"], "error": {"code": -32000, "message": "PROCESS_SECRET_DO_NOT_RECORD"}})
shutdown = read()
assert shutdown["method"] == "shutdown"
write({"jsonrpc": "2.0", "id": shutdown["id"], "result": {}})
exit_message = read()
assert exit_message["method"] == "exit"
"#,
    );
    let error = execute(
        &extension(temp.path(), "exercise", Vec::new()),
        "exercise",
        &FakeHost::default(),
    )
    .expect_err("process error");
    let message = error.to_string();

    assert_eq!(
        message,
        ManagedProcessRuntimeError::CommandFailed.to_string()
    );
    assert!(!message.contains("PROCESS_SECRET_DO_NOT_RECORD"));
}

#[test]
fn peer_messages_queue_while_a_host_call_is_running() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        r#"import json
import sys

def read():
    return json.loads(sys.stdin.buffer.readline())

def write(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()

initialize = read()
write({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocol_version": "euler-managed-process/1"}})
read()
command = read()
write({"jsonrpc": "2.0", "id": "slow-host-call", "method": "euler/host/query-provenance", "params": {"after_event_id": None, "kinds": [], "limit": 2, "scan_limit": 4, "include_blob_fields": False, "blob_byte_limit": 1024}})
for index in range(33):
    write({"jsonrpc": "2.0", "method": "euler/progress", "params": {"message": f"queued-{index}", "fraction": 0.5}})
write({"jsonrpc": "2.0", "id": command["id"], "result": {"queued": 33}})
host_response = read()
assert host_response["id"] == "slow-host-call"
shutdown = read()
assert shutdown["method"] == "shutdown"
write({"jsonrpc": "2.0", "id": shutdown["id"], "result": {}})
exit_message = read()
assert exit_message["method"] == "exit"
"#,
    );
    let host = FakeHost {
        query_delay: Some(Duration::from_millis(75)),
        ..FakeHost::default()
    };
    let output = execute(
        &extension(
            temp.path(),
            "exercise",
            vec![Capability::ProvenanceRead.as_str().to_owned()],
        ),
        "exercise",
        &host,
    )
    .expect("queued peer messages must remain within the declared budget");

    assert_eq!(output, json!({"queued": 33}));
}

#[test]
fn shutdown_requires_an_object_result() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        r#"import json
import sys

def read():
    return json.loads(sys.stdin.buffer.readline())

def write(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

initialize = read()
write({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocol_version": "euler-managed-process/1"}})
read()
command = read()
write({"jsonrpc": "2.0", "id": command["id"], "result": {"ok": True}})
shutdown = read()
assert shutdown["method"] == "shutdown"
write({"jsonrpc": "2.0", "id": shutdown["id"], "result": None})
"#,
    );
    let error = execute(
        &extension(temp.path(), "exercise", Vec::new()),
        "exercise",
        &FakeHost::default(),
    )
    .expect_err("non-object shutdown response");

    assert_eq!(
        error.to_string(),
        ManagedProcessRuntimeError::ProtocolViolation.to_string()
    );
}

#[test]
fn nonzero_final_child_status_is_a_generic_extension_failure() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        r#"import json
import sys

def read():
    return json.loads(sys.stdin.buffer.readline())

def write(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

initialize = read()
write({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocol_version": "euler-managed-process/1"}})
read()
command = read()
write({"jsonrpc": "2.0", "id": command["id"], "result": {"ok": True}})
shutdown = read()
write({"jsonrpc": "2.0", "id": shutdown["id"], "result": {}})
exit_message = read()
assert exit_message["method"] == "exit"
raise SystemExit(7)
"#,
    );
    let error = execute(
        &extension(temp.path(), "exercise", Vec::new()),
        "exercise",
        &FakeHost::default(),
    )
    .expect_err("nonzero peer exit");

    assert_eq!(
        error.to_string(),
        ManagedProcessRuntimeError::CommandFailed.to_string()
    );
}

#[test]
fn timed_out_python_command_is_cancelled_and_reaped() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        &sdk_script(
            r#"
import time

def wait_forever(context):
    time.sleep(10)
    return {"unexpected": True}

serve({"wait": wait_forever})
"#,
        ),
    );
    let limits = ManagedProcessLimits {
        invocation_timeout: Duration::from_millis(40),
        cancel_grace: Duration::from_millis(20),
        ..ManagedProcessLimits::default()
    };
    let extension = extension(temp.path(), "wait", Vec::new()).with_limits(limits);
    let start = Instant::now();
    let error = execute(&extension, "wait", &FakeHost::default()).expect_err("timeout");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "process was not reaped promptly"
    );
    assert!(
        error
            .to_string()
            .contains("did not complete its command in time"),
        "error: {error}"
    );
}

#[test]
fn large_command_input_to_a_peer_that_stops_reading_is_bounded() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        r#"import json
import sys
import time

initialize = json.loads(sys.stdin.buffer.readline())
sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocol_version": "euler-managed-process/1"}}) + "\n")
sys.stdout.flush()
sys.stdin.buffer.readline()
time.sleep(10)
"#,
    );
    let limits = ManagedProcessLimits {
        invocation_timeout: Duration::from_millis(40),
        cancel_grace: Duration::from_millis(20),
        ..ManagedProcessLimits::default()
    };
    let extension = extension(temp.path(), "exercise", Vec::new()).with_limits(limits);
    let input = json!({"payload": "x".repeat(512 * 1024)});
    let start = Instant::now();
    let error = execute_with_input(&extension, "exercise", input, &FakeHost::default())
        .expect_err("timeout");

    assert!(
        start.elapsed() < Duration::from_secs(2),
        "a blocked stdin writer held the invocation open"
    );
    assert_eq!(
        error.to_string(),
        ManagedProcessRuntimeError::InvocationTimedOut.to_string()
    );
}

#[test]
fn host_response_to_a_peer_that_stops_reading_is_bounded() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        r#"import json
import sys
import time

def read():
    return json.loads(sys.stdin.buffer.readline())

def write(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

initialize = read()
write({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocol_version": "euler-managed-process/1"}})
read()
read()
write({"jsonrpc": "2.0", "id": "large-host-response", "method": "euler/host/read-diagnostics", "params": {"tail_lines": 1, "max_bytes": 700000}})
time.sleep(10)
"#,
    );
    let limits = ManagedProcessLimits {
        invocation_timeout: Duration::from_millis(40),
        cancel_grace: Duration::from_millis(20),
        ..ManagedProcessLimits::default()
    };
    let extension = extension(
        temp.path(),
        "exercise",
        vec![Capability::DiagnosticsRead.as_str().to_owned()],
    )
    .with_limits(limits);
    let host = FakeHost {
        diagnostics_line: "x".repeat(512 * 1024),
        ..FakeHost::default()
    };
    let start = Instant::now();
    let error = execute(&extension, "exercise", &host).expect_err("timeout");

    assert!(
        start.elapsed() < Duration::from_secs(2),
        "a blocked host response held the invocation open"
    );
    assert_eq!(
        error.to_string(),
        ManagedProcessRuntimeError::InvocationTimedOut.to_string()
    );
}

#[cfg(unix)]
#[test]
fn timeout_terminates_descendants_that_inherit_protocol_pipes() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        &sdk_script(
            r#"
import subprocess
import sys
import time

def wait(context):
    subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
    time.sleep(30)
    return {"unexpected": True}

serve({"wait": wait})
"#,
        ),
    );
    let limits = ManagedProcessLimits {
        invocation_timeout: Duration::from_millis(40),
        cancel_grace: Duration::from_millis(20),
        ..ManagedProcessLimits::default()
    };
    let extension = extension(temp.path(), "wait", Vec::new()).with_limits(limits);
    let start = Instant::now();
    let error = execute(&extension, "wait", &FakeHost::default()).expect_err("timeout");

    assert!(
        start.elapsed() < Duration::from_secs(2),
        "descendant-held protocol pipes blocked cleanup"
    );
    assert_eq!(
        error.to_string(),
        ManagedProcessRuntimeError::InvocationTimedOut.to_string()
    );
}

#[test]
fn timeout_sends_a_json_rpc_cancellation_before_reaping_the_peer() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    let marker = temp.path().join("cancelled");
    write_script(
        temp.path(),
        "extension.py",
        r#"import json
import sys
from pathlib import Path

def read():
    line = sys.stdin.buffer.readline()
    if not line:
        raise SystemExit(2)
    return json.loads(line)

def write(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

initialize = read()
write({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocol_version": "euler-managed-process/1"}})
read()
read()
cancellation = read()
if cancellation.get("method") == "$/cancelRequest" and cancellation.get("params", {}).get("id") == "euler-command-1":
    Path("cancelled").write_text("yes", encoding="utf-8")
"#,
    );
    let limits = ManagedProcessLimits {
        invocation_timeout: Duration::from_millis(40),
        cancel_grace: Duration::from_millis(250),
        ..ManagedProcessLimits::default()
    };
    let extension = extension(temp.path(), "exercise", Vec::new()).with_limits(limits);
    let error = execute(&extension, "exercise", &FakeHost::default()).expect_err("timeout");

    assert_eq!(
        error.to_string(),
        ManagedProcessRuntimeError::InvocationTimedOut.to_string()
    );
    assert!(
        marker.is_file(),
        "peer did not observe the cancellation request"
    );
}

#[test]
fn oversized_protocol_line_is_rejected_before_json_allocation() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        "import sys\nsys.stdin.buffer.readline()\nsys.stdout.write('x' * 2048)\nsys.stdout.flush()\n",
    );
    let limits = ManagedProcessLimits {
        max_message_bytes: 512,
        ..ManagedProcessLimits::default()
    };
    let extension = extension(temp.path(), "exercise", Vec::new()).with_limits(limits);
    let error = execute(&extension, "exercise", &FakeHost::default()).expect_err("output limit");
    assert_eq!(
        error.to_string(),
        ManagedProcessRuntimeError::OutputLimitExceeded.to_string()
    );
}

#[test]
fn excessive_stderr_is_bounded_and_never_returned_to_the_host() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        "import sys, time\nsys.stderr.write('PROCESS_SECRET_DO_NOT_RECORD' * 64)\nsys.stderr.flush()\ntime.sleep(10)\n",
    );
    let limits = ManagedProcessLimits {
        max_stderr_bytes: 128,
        cancel_grace: Duration::from_millis(20),
        ..ManagedProcessLimits::default()
    };
    let extension = extension(temp.path(), "exercise", Vec::new()).with_limits(limits);
    let start = Instant::now();
    let error = execute(&extension, "exercise", &FakeHost::default()).expect_err("stderr limit");

    assert!(
        start.elapsed() < Duration::from_secs(2),
        "process was not reaped promptly"
    );
    assert_eq!(
        error.to_string(),
        ManagedProcessRuntimeError::OutputLimitExceeded.to_string()
    );
    assert!(!error.to_string().contains("PROCESS_SECRET_DO_NOT_RECORD"));
}

#[test]
fn aggregate_protocol_output_is_bounded_across_valid_messages() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        r#"import json
import sys
import time

def read():
    return json.loads(sys.stdin.buffer.readline())

def write(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

initialize = read()
write({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocol_version": "euler-managed-process/1"}})
read()
read()
for _ in range(10):
    write({"jsonrpc": "2.0", "method": "euler/progress", "params": {"message": "bounded aggregate protocol output", "fraction": 0.5}})
time.sleep(10)
"#,
    );
    let limits = ManagedProcessLimits {
        max_protocol_bytes: 512,
        max_protocol_messages: 32,
        max_progress_messages: 32,
        max_progress_bytes: 4096,
        cancel_grace: Duration::from_millis(20),
        ..ManagedProcessLimits::default()
    };
    let extension = extension(temp.path(), "exercise", Vec::new()).with_limits(limits);
    let error = execute(&extension, "exercise", &FakeHost::default()).expect_err("output limit");

    assert_eq!(
        error.to_string(),
        ManagedProcessRuntimeError::OutputLimitExceeded.to_string()
    );
}

#[test]
fn host_request_count_is_bounded_even_when_the_peer_reads_every_response() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        r#"import json
import sys
import time

def read():
    return json.loads(sys.stdin.buffer.readline())

def write(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

initialize = read()
write({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocol_version": "euler-managed-process/1"}})
read()
read()
for index in range(3):
    write({"jsonrpc": "2.0", "id": f"state-{index}", "method": "euler/host/state-dir", "params": {}})
    read()
time.sleep(10)
"#,
    );
    let limits = ManagedProcessLimits {
        max_host_requests: 2,
        cancel_grace: Duration::from_millis(20),
        ..ManagedProcessLimits::default()
    };
    let extension = extension(
        temp.path(),
        "exercise",
        vec![Capability::FsWrite.as_str().to_owned()],
    )
    .with_limits(limits);
    let error =
        execute(&extension, "exercise", &FakeHost::default()).expect_err("host request limit");

    assert_eq!(
        error.to_string(),
        ManagedProcessRuntimeError::OutputLimitExceeded.to_string()
    );
}

#[test]
fn python_sdk_accepts_a_command_frame_at_the_advertised_size_boundary() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        &sdk_script(
            r#"
def exercise(context):
    return {"padding_bytes": len(context.input["padding"])}

serve({"exercise": exercise})
"#,
        ),
    );
    let limits = ManagedProcessLimits {
        max_message_bytes: 512,
        ..ManagedProcessLimits::default()
    };
    let input = boundary_sized_input(limits.max_message_bytes);
    let extension = extension(temp.path(), "exercise", Vec::new()).with_limits(limits);
    let output = execute_with_input(&extension, "exercise", input.clone(), &FakeHost::default())
        .expect("boundary-sized command");

    assert_eq!(
        output["padding_bytes"],
        json!(input["padding"].as_str().unwrap().len())
    );
}

#[test]
fn host_capability_denial_stays_host_owned_and_is_not_process_output() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        &sdk_script(
            r#"
def write(context):
    context.host.write_artifact(
        display_name="denied.txt",
        media_type="text/plain",
        data=b"must not persist",
    )
    return {"unexpected": True}

serve({"write": write})
"#,
        ),
    );
    let extension = extension(
        temp.path(),
        "write",
        vec![Capability::ArtifactWrite.as_str().to_owned()],
    );
    let host = FakeHost {
        deny_artifact: true,
        ..FakeHost::default()
    };
    let error = execute(&extension, "write", &host).expect_err("capability denial");

    assert_eq!(
        error.to_string(),
        ManagedProcessRuntimeError::CommandFailed.to_string()
    );
    assert!(!error.to_string().contains("artifact-write"));
    assert!(host.artifacts.borrow().is_empty());
}

#[test]
fn raw_json_rpc_peer_conforms_without_using_the_python_sdk() {
    require_python();
    let temp = TempDir::new().expect("temp package");
    write_script(
        temp.path(),
        "extension.py",
        r#"import json
import sys

def read():
    line = sys.stdin.buffer.readline()
    if not line:
        raise SystemExit(2)
    return json.loads(line)

def write(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()

initialize = read()
assert initialize["jsonrpc"] == "2.0"
assert initialize["method"] == "initialize"
assert "euler-managed-process/1" in initialize["params"]["protocol_versions"]
write({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocol_version": "euler-managed-process/1"}})

initialized = read()
assert initialized["method"] == "initialized"
command = read()
assert command["method"] == "euler/command"
assert command["params"]["command"] == "exercise"

write({"jsonrpc": "2.0", "id": "peer-query", "method": "euler/host/query-provenance", "params": {"after_event_id": None, "kinds": [], "limit": 2, "scan_limit": 4, "include_blob_fields": False, "blob_byte_limit": 1024}})
query_response = read()
assert query_response["id"] == "peer-query"
assert query_response["result"]["applied_limit"] == 2
write({"jsonrpc": "2.0", "id": "peer-invalid", "method": "euler/host/query-provenance", "params": {"after_event_id": None, "kinds": [], "limit": 2, "scan_limit": 4, "include_blob_fields": False, "blob_byte_limit": 1024, "unexpected": True}})
invalid_response = read()
assert invalid_response["id"] == "peer-invalid"
assert invalid_response["error"]["code"] == -32602
write({"jsonrpc": "2.0", "id": "peer-unknown", "method": "euler/host/not-real", "params": {}})
unknown_response = read()
assert unknown_response["id"] == "peer-unknown"
assert unknown_response["error"]["code"] == -32601
write({"jsonrpc": "2.0", "method": "euler/progress", "params": {"message": "raw protocol peer", "fraction": 0.5}})
write({"jsonrpc": "2.0", "id": command["id"], "result": {"implementation": "raw-json-rpc", "event_count": len(query_response["result"]["events"])}})

shutdown = read()
assert shutdown["method"] == "shutdown"
write({"jsonrpc": "2.0", "id": shutdown["id"], "result": {}})
exit_message = read()
assert exit_message["method"] == "exit"
"#,
    );
    let extension = extension(
        temp.path(),
        "exercise",
        vec![Capability::ProvenanceRead.as_str().to_owned()],
    );
    let output = execute(&extension, "exercise", &FakeHost::default()).expect("raw peer result");

    assert_eq!(
        output,
        json!({"implementation": "raw-json-rpc", "event_count": 0})
    );
}

fn extension(
    package_dir: &Path,
    command: &str,
    capabilities: Vec<String>,
) -> ManagedProcessExtension {
    let descriptor = StaticExtensionDescriptor {
        id: "python-proof".to_owned(),
        display_name: "Python proof".to_owned(),
        version: "0.1.1".to_owned(),
        runtime_kind: "managed-process".to_owned(),
        capabilities: capabilities.clone(),
        commands: vec![StaticCommandDescriptor {
            name: command.to_owned(),
            display_name: command.to_owned(),
            summary: "test command".to_owned(),
            required_capabilities: capabilities,
            invocation: euler_sdk::Invocation::User,
        }],
    };
    ManagedProcessExtension::new(
        package_dir,
        &descriptor,
        ManagedProcessEntrypoint {
            command: vec![
                "python3".to_owned(),
                "-B".to_owned(),
                "-u".to_owned(),
                "extension.py".to_owned(),
            ],
        },
    )
    .expect("managed extension")
}

fn execute(
    extension: &ManagedProcessExtension,
    command: &str,
    host: &dyn HostApi,
) -> Result<Value, ExtensionError> {
    execute_with_input(extension, command, json!({}), host)
}

fn execute_with_input(
    extension: &ManagedProcessExtension,
    command: &str,
    input: Value,
    host: &dyn HostApi,
) -> Result<Value, ExtensionError> {
    let mut registrar = TestRegistrar::default();
    extension.register(&mut registrar).expect("register");
    let command = registrar
        .commands
        .into_iter()
        .find(|(name, _)| name == command)
        .expect("command")
        .1;
    command.execute(CommandContext { input }, host)
}

fn boundary_sized_input(max_message_bytes: usize) -> Value {
    let empty = json!({
        "jsonrpc": "2.0",
        "id": "euler-command-1",
        "method": "euler/command",
        "params": {"command": "exercise", "input": {"padding": ""}},
    });
    let overhead = serde_json::to_vec(&empty)
        .expect("serialize empty command frame")
        .len();
    let padding = "x".repeat(max_message_bytes.saturating_sub(overhead));
    let input = json!({"padding": padding});
    let frame = json!({
        "jsonrpc": "2.0",
        "id": "euler-command-1",
        "method": "euler/command",
        "params": {"command": "exercise", "input": input},
    });
    assert_eq!(
        serde_json::to_vec(&frame)
            .expect("serialize boundary command")
            .len(),
        max_message_bytes
    );
    frame["params"]["input"].clone()
}

#[derive(Default)]
struct TestRegistrar {
    commands: Vec<(String, Box<dyn ExtensionCommand>)>,
}

impl CommandRegistrar for TestRegistrar {
    fn register_command(&mut self, name: &str, command: Box<dyn ExtensionCommand>) {
        self.commands.push((name.to_owned(), command));
    }
}

#[derive(Default)]
struct FakeHost {
    checkpoint: RefCell<Option<EventFeedCheckpoint>>,
    artifacts: RefCell<Vec<ArtifactWrite>>,
    slots: RefCell<Vec<(String, String)>>,
    recorded_agent_tasks: RefCell<usize>,
    spawned_agents: RefCell<usize>,
    spawned_batches: RefCell<usize>,
    deny_artifact: bool,
    diagnostics_line: String,
    query_delay: Option<Duration>,
}

impl HostApi for FakeHost {
    fn query_provenance(&self, _query: ProvenanceQuery) -> Result<ProvenancePage, ExtensionError> {
        if let Some(delay) = self.query_delay {
            std::thread::sleep(delay);
        }
        Ok(ProvenancePage {
            events: Vec::new(),
            applied_limit: 2,
            applied_scan_limit: 4,
            scanned_events: 0,
            watermark_event_id: None,
            next_after_event_id: None,
            truncated: false,
        })
    }

    fn read_diagnostics(
        &self,
        _query: DiagnosticsQuery,
    ) -> Result<DiagnosticsPage, ExtensionError> {
        Ok(DiagnosticsPage {
            lines: vec![if self.diagnostics_line.is_empty() {
                "diagnostic line".to_owned()
            } else {
                self.diagnostics_line.clone()
            }],
            truncated: false,
        })
    }

    fn state_dir(&self) -> Result<PathBuf, ExtensionError> {
        Ok(PathBuf::from("/private/state"))
    }

    fn write_artifact(&self, artifact: ArtifactWrite) -> Result<ArtifactRecord, ExtensionError> {
        if self.deny_artifact {
            return Err(ExtensionError::CapabilityDenied {
                capability: Capability::ArtifactWrite,
            });
        }
        self.artifacts.borrow_mut().push(artifact);
        Ok(ArtifactRecord {
            persisted_event_id: "artifact-event".to_owned(),
            relative_path: "extensions/python-proof/artifacts/result".to_owned(),
            sha256: "hash".to_owned(),
            byte_len: 15,
        })
    }

    fn load_event_feed_checkpoint(
        &self,
        _name: &str,
    ) -> Result<Option<EventFeedCheckpoint>, ExtensionError> {
        Ok(self.checkpoint.borrow().clone())
    }

    fn store_event_feed_checkpoint(
        &self,
        _name: &str,
        checkpoint: EventFeedCheckpoint,
    ) -> Result<(), ExtensionError> {
        *self.checkpoint.borrow_mut() = Some(checkpoint);
        Ok(())
    }

    fn record_agent_task_result(
        &self,
        _task: HostAgentTask,
        _result: HostAgentResult,
    ) -> Result<HostAgentRecord, ExtensionError> {
        *self.recorded_agent_tasks.borrow_mut() += 1;
        Ok(HostAgentRecord {
            child_agent_id: "child-record".to_owned(),
            spawn_event_id: "spawn-event".to_owned(),
            result_event_id: "result-event".to_owned(),
        })
    }

    fn update_context_slot(&self, slot: &str, content: &str) -> Result<(), ExtensionError> {
        self.slots
            .borrow_mut()
            .push((slot.to_owned(), content.to_owned()));
        Ok(())
    }

    fn spawn_agent(&self, _task: SpawnAgentTask) -> Result<AgentOutcome, ExtensionError> {
        *self.spawned_agents.borrow_mut() += 1;
        Ok(agent_outcome("child-live"))
    }

    fn spawn_agents(
        &self,
        tasks: Vec<SpawnAgentTask>,
    ) -> Result<Vec<AgentOutcome>, ExtensionError> {
        *self.spawned_batches.borrow_mut() += 1;
        Ok(tasks
            .iter()
            .map(|_| agent_outcome("child-live-many"))
            .collect())
    }
}

fn agent_outcome(child_agent_id: &str) -> AgentOutcome {
    AgentOutcome {
        ok: true,
        summary: "done".to_owned(),
        output: "body".to_owned(),
        error: None,
        provider: "fixture".to_owned(),
        model: "fixture".to_owned(),
        child_agent_id: child_agent_id.to_owned(),
        spawn_event_id: "spawn-live".to_owned(),
        result_event_id: "result-live".to_owned(),
    }
}

fn all_capabilities() -> Vec<String> {
    [
        Capability::ProvenanceRead,
        Capability::DiagnosticsRead,
        Capability::FsRead,
        Capability::FsWrite,
        Capability::ArtifactWrite,
        Capability::AgentRecord,
        Capability::AgentSpawn,
        Capability::ContextSlot,
    ]
    .into_iter()
    .map(|capability| capability.as_str().to_owned())
    .collect()
}

fn require_python() {
    let output = Command::new("python3")
        .arg("--version")
        .output()
        .expect("Python 3 is required for the managed-process Python SDK test");
    assert!(output.status.success(), "python3 --version failed");
}

fn write_script(package_dir: &Path, name: &str, content: &str) {
    fs::write(package_dir.join(name), content).expect("write Python extension script");
}

fn sdk_script(body: &str) -> String {
    let sdk_source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../python/euler_managed_process_sdk/src");
    let sdk_source =
        serde_json::to_string(&sdk_source.to_string_lossy()).expect("serialize SDK source path");
    format!(
        "import sys\nsys.path.insert(0, {sdk_source})\nfrom euler_managed_process_sdk import serve\n{body}"
    )
}
