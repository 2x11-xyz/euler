#![allow(clippy::too_many_lines)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use euler_event::{object, EventEnvelope, EventKind};
use portable_pty::{native_pty_system, Child, CommandBuilder, PtySize};

#[test]
fn fixture_loop_writes_jsonl_in_rendered_order() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let mut child = command_with_home(exe, &home)
        .arg("--provenance")
        .arg(&log)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"hello skeleton\n")
        .expect("write stdin");

    let output = child.wait_with_output().expect("wait for euler");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert_eq!(
        stdout,
        "user: hello skeleton\nmodel.call: fixture/echo\nmodel.result: user: hello skeleton\nassistant: user: hello skeleton\n"
    );

    assert!(
        !home.path().join(".euler").join("sessions").exists(),
        "explicit --provenance should not create a home session"
    );

    let jsonl = fs::read_to_string(&log).expect("read jsonl");
    let lines: Vec<&str> = jsonl.lines().collect();
    assert_eq!(lines.len(), 7);
    assert!(lines[0].contains("\"kind\":\"session.start\""));
    // Dormant project-context bootstrap (ADR 0017): one disabled snapshot,
    // no diagnostics in an empty temp workspace, and no source bodies.
    assert!(lines[1].contains("\"kind\":\"project.context.snapshot\""));
    assert!(lines[1].contains("\"status\":\"disabled\""));
    assert!(lines[2].contains("\"kind\":\"user.message\""));
    assert!(lines[3].contains("\"kind\":\"canvas.snapshot\""));
    assert!(lines[4].contains("\"kind\":\"model.call\""));
    assert!(lines[5].contains("\"kind\":\"model.result\""));
    assert!(lines[6].contains("\"kind\":\"assistant.message\""));

    assert!(lines[2].contains("\"content\":\"hello skeleton\""));
    assert!(lines[4].contains("\"provider\":\"fixture\""));
    assert!(lines[4].contains("\"model\":\"echo\""));
    assert!(lines[5].contains("\"content\":\"user: hello skeleton\""));
    assert!(lines[6].contains("\"content\":\"user: hello skeleton\""));
}

#[test]
fn agent_shell_isolates_nested_euler_home_and_preserves_rust_log() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let user_home = isolated_home();
    let outer_euler_home = tempfile::tempdir().expect("outer Euler home");
    let workspace = tempfile::tempdir().expect("workspace");
    let extension_dir = tempfile::tempdir().expect("extension dir");
    write_extension_manifest(extension_dir.path(), "default-only-extension", "0.1.0");

    let linked = command_with_home(exe, &user_home)
        .env_remove("EULER_HOME")
        .args(["extension", "link", path_str(extension_dir.path())])
        .output()
        .expect("link extension in default home");
    assert!(
        linked.status.success(),
        "link stderr: {}",
        String::from_utf8_lossy(&linked.stderr)
    );
    let default_list = command_with_home(exe, &user_home)
        .env_remove("EULER_HOME")
        .args(["extension", "list"])
        .output()
        .expect("list default-home extensions");
    assert!(
        default_list.status.success(),
        "default list stderr: {}",
        String::from_utf8_lossy(&default_list.stderr)
    );
    assert!(String::from_utf8_lossy(&default_list.stdout).contains("default-only-extension"));

    let nested_command = format!(
        "printf 'rust-log=%s\\nchild-home=%s\\n' \"$RUST_LOG\" \"$EULER_HOME\"; {} extension list",
        shell_quote(exe)
    );
    let script = write_fixture_script(
        workspace.path(),
        "nested-home.json",
        &serde_json::json!({
            "version": 1,
            "responses": [
                {"events": [
                    {"tool_call": {
                        "id": "call-nested-euler",
                        "name": "run_shell",
                        "input": {"command": nested_command}
                    }},
                    {"finished": {"stop_reason": "tool_use"}}
                ]},
                {"events": [
                    {"text_delta": "done"},
                    {"finished": {"stop_reason": "completed"}}
                ]}
            ]
        })
        .to_string(),
    );
    let log = workspace.path().join("events.jsonl");
    let output = command_with_home(exe, &user_home)
        .current_dir(workspace.path())
        .env("EULER_HOME", outer_euler_home.path())
        .env("RUST_LOG", "project_under_test=trace")
        .env_remove("EULER_PROVIDER")
        .env_remove("EULER_MODEL")
        .args([
            "exec",
            "--provider",
            "fixture",
            "--provider-option",
            &format!("event-script={}", path_str(&script)),
            "--extensions",
            "none",
            "--project-context",
            "off",
            "--auto-approve",
            "trusted-local",
            "--provenance",
            path_str(&log),
            "check nested Euler isolation",
        ])
        .output()
        .expect("run outer Euler");
    assert!(
        output.status.success(),
        "outer stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = read_jsonl(&log);
    let tool_output = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::TOOL_RESULT)
        .and_then(|event| event.payload.get("output"))
        .and_then(serde_json::Value::as_str)
        .expect("nested Euler tool output");
    assert!(tool_output.contains("rust-log=project_under_test=trace"));
    assert!(!tool_output.contains("default-only-extension"));
    let child_home = tool_output
        .lines()
        .find_map(|line| line.strip_prefix("child-home="))
        .map(PathBuf::from)
        .expect("isolated child Euler home");
    assert!(child_home.is_absolute());
    assert_ne!(child_home, outer_euler_home.path());
    assert_ne!(child_home, user_home.path().join(".euler"));
    assert!(
        !child_home.exists(),
        "temporary child home should be removed"
    );
}

#[test]
fn headless_session_writes_stable_diagnostics_jsonl() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let root = tempfile::tempdir().expect("root dir");
    let log = root.path().join("events.jsonl");
    let script = write_fixture_script(
        root.path(),
        "diagnostics-script.json",
        r#"{
  "version": 1,
  "responses": [
    {
      "events": [
        {
          "tool_call": {
            "id": "call-read",
            "name": "read_file",
            "input": { "path": "diagnostics-input.txt" }
          }
        },
        { "finished": { "stop_reason": "tool_use" } }
      ]
    },
    {
      "events": [
        { "text_delta": "done" },
        { "finished": { "stop_reason": "completed" } }
      ]
    }
  ]
}
"#,
    );
    fs::write(root.path().join("diagnostics-input.txt"), "hello\n").expect("write input");

    let mut child = command_with_home(exe, &home)
        .current_dir(root.path())
        .arg("--provider")
        .arg("fixture")
        .arg("--provider-option")
        .arg(format!("event-script={}", path_str(&script)))
        .arg("--provenance")
        .arg(path_str(&log))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"diagnose this\n")
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait euler");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let lines = read_diagnostics_jsonl(&root.path().join("diagnostics.jsonl"));
    assert!(lines.len() > 3, "expected multiple diagnostics lines");
    assert!(lines
        .iter()
        .all(|line| line["session_id"] == "headless-session"));
    assert_schema(&lines[0]);
    assert!(has_diagnostic_event(&lines, "model_call_end"));
    assert!(has_diagnostic_event(&lines, "tool_exec_end"));
    assert!(has_diagnostic_event(&lines, "permission_decision"));
}

#[test]
fn diagnostics_bind_failure_does_not_fail_headless_session() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let root = tempfile::tempdir().expect("root dir");
    let missing_parent = root.path().join("missing-parent");
    let log = missing_parent.join("events.jsonl");

    let output = command_with_home(exe, &home)
        .current_dir(root.path())
        .arg("exec")
        .arg("--provider")
        .arg("fixture")
        .arg("--provenance")
        .arg(path_str(&log))
        .arg("hello diagnostics")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run exec");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(log.is_file(), "provenance still written");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("diagnostics logging disabled"),
        "expected diagnostics warning on stderr"
    );
}

#[cfg(unix)]
#[test]
fn exec_observe_runs_enabled_linked_python_observer_automatically() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let root = tempfile::tempdir().expect("root dir");
    let extension_dir = tempfile::tempdir().expect("extension dir");
    let sdk_source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../python/euler_managed_process_sdk/src");
    let manifest = serde_json::json!({
        "version": 1,
        "id": "python-round-observer",
        "display_name": "Python round observer",
        "extension_version": "0.1.0",
        "runtime_kind": "managed-process",
        "entrypoint": {"command": ["python3", "-B", "-u", "extension.py"]},
        "capabilities": [],
        "commands": [
            {
                "name": "observer-brief",
                "display_name": "Observer brief",
                "summary": "Observe a round boundary.",
                "required_capabilities": []
            },
            {
                "name": "observer-apply",
                "display_name": "Observer apply",
                "summary": "Apply an observation.",
                "required_capabilities": []
            }
        ],
        "observer": {
            "brief_command": "observer-brief",
            "apply_command": "observer-apply",
            "default_cadence_rounds": 1
        }
    });
    fs::write(
        extension_dir
            .path()
            .join(euler_core::EXTENSION_MANIFEST_FILE),
        serde_json::to_vec_pretty(&manifest).expect("manifest json"),
    )
    .expect("write observer manifest");
    fs::write(
        extension_dir.path().join("extension.py"),
        format!(
            r#"import sys
+from pathlib import Path
+sys.path.insert(0, {sdk_source:?})
+from euler_managed_process_sdk import serve
+
+def brief(context):
+    Path("observer-ran").write_text("yes")
+    return {{"status": "idle"}}
+
+serve({{"observer-brief": brief, "observer-apply": lambda context: {{"ok": True}}}})
+"#,
            sdk_source = sdk_source.to_string_lossy()
        )
        .replace("\n+", "\n"),
    )
    .expect("write observer process");
    configure_linked_extension(exe, &home, extension_dir.path(), "python-round-observer");
    // Corrupt only the duplicated inventory descriptor. Runtime observer
    // selection must come from the hash-checked source manifest, not links.json.
    let inventory_path = home.path().join(".euler/extensions/links.json");
    let mut inventory: serde_json::Value =
        serde_json::from_slice(&fs::read(&inventory_path).expect("read inventory"))
            .expect("inventory json");
    inventory["links"]["python-round-observer"]["descriptor"]["observer"]
        ["default_cadence_rounds"] = serde_json::json!(0);
    inventory["links"]["python-round-observer"]["descriptor"]["observer"]["brief_command"] =
        serde_json::json!("observer-apply");
    fs::write(
        &inventory_path,
        serde_json::to_vec_pretty(&inventory).expect("serialize tampered inventory"),
    )
    .expect("write tampered inventory");
    fs::write(root.path().join("input.txt"), "observer input\n").expect("write input");
    let script = write_fixture_script(
        root.path(),
        "python-observer-loop.json",
        &r#"{
+  "version": 1,
+  "responses": [
+    {"events": [
+      {"tool_call": {"id": "call-read", "name": "read_file", "input": {"path": "input.txt"}}},
+      {"finished": {"stop_reason": "tool_use"}}
+    ]},
+    {"events": [
+      {"text_delta": "driver done"},
+      {"finished": {"stop_reason": "completed"}}
+    ]}
+  ]
+}"#
        .replace("\n+", "\n"),
    );

    let output = command_with_home(exe, &home)
        .current_dir(root.path())
        .args([
            "exec",
            "--provider",
            "fixture",
            "--provider-option",
            &format!("event-script={}", path_str(&script)),
            "--observe",
            "python-round-observer",
            "read input.txt",
        ])
        .output()
        .expect("run exec with Python observer");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        extension_dir.path().join("observer-ran").is_file(),
        "managed observer brief did not run automatically"
    );
}

#[cfg(unix)]
#[test]
fn exec_observe_rejects_linked_python_observer_without_launch_consent() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let extension_dir = tempfile::tempdir().expect("extension dir");
    let manifest = serde_json::json!({
        "version": 1,
        "id": "python-disabled-observer",
        "display_name": "Python disabled observer",
        "extension_version": "0.1.0",
        "runtime_kind": "managed-process",
        "entrypoint": {"command": ["python3", "-B", "-u", "extension.py"]},
        "capabilities": [],
        "commands": [
            {"name":"brief","display_name":"Brief","summary":"Brief.","required_capabilities":[]},
            {"name":"apply","display_name":"Apply","summary":"Apply.","required_capabilities":[]}
        ],
        "observer": {
            "brief_command": "brief",
            "apply_command": "apply",
            "default_cadence_rounds": 1
        }
    });
    fs::write(
        extension_dir
            .path()
            .join(euler_core::EXTENSION_MANIFEST_FILE),
        serde_json::to_vec_pretty(&manifest).expect("manifest json"),
    )
    .expect("write observer manifest");
    fs::write(
        extension_dir.path().join("extension.py"),
        "from pathlib import Path\nPath('observer-ran').write_text('yes')\n",
    )
    .expect("write observer process");
    let linked = command_with_home(exe, &home)
        .args(["extension", "link", path_str(extension_dir.path())])
        .output()
        .expect("link observer");
    assert!(linked.status.success());

    let output = command_with_home(exe, &home)
        .args([
            "exec",
            "--provider",
            "fixture",
            "--observe",
            "python-disabled-observer",
            "do work",
        ])
        .output()
        .expect("reject disabled observer");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(
        "linked extension is not enabled; run `euler extension enable python-disabled-observer` first"
    ));
    assert!(!extension_dir.path().join("observer-ran").exists());
}

#[test]
fn exec_renders_each_turn_event_to_stdout_in_order() {
    // Regression for #7: exec stdout is produced per event (streamed +
    // flushed as the turn runs), so every event of the turn — not just the
    // final assistant line — reaches stdout, in emission order. Provenance
    // stays the canonical detailed stream.
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let root = tempfile::tempdir().expect("root dir");
    let log = root.path().join("events.jsonl");
    fs::write(root.path().join("note.txt"), "alpha\n").expect("write note");
    let script = write_fixture_script(
        root.path(),
        "exec-stream.json",
        r#"{
  "version": 1,
  "responses": [
    {
      "events": [
        { "tool_call": { "id": "c1", "name": "read_file", "input": { "path": "note.txt" } } },
        { "finished": { "stop_reason": "tool_use" } }
      ]
    },
    {
      "events": [
        { "text_delta": "all done" },
        { "finished": { "stop_reason": "completed" } }
      ]
    }
  ]
}
"#,
    );

    let output = command_with_home(exe, &home)
        .current_dir(root.path())
        .arg("exec")
        .arg("--provider")
        .arg("fixture")
        .arg("--provider-option")
        .arg(format!("event-script={}", path_str(&script)))
        .arg("--provenance")
        .arg(path_str(&log))
        .arg("--auto-approve")
        .arg("read-only")
        .arg("summarize note")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run exec");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let user_at = stdout
        .find("user: summarize note")
        .expect("user line streamed");
    let read_at = stdout
        .find("tool.call: read_file")
        .expect("tool call streamed");
    let assistant_at = stdout
        .find("assistant: all done")
        .expect("assistant line streamed");
    // The whole turn is on stdout, in order — not just the final response.
    assert!(user_at < read_at, "user before tool: {stdout}");
    assert!(read_at < assistant_at, "tool before assistant: {stdout}");
}

#[test]
fn exec_streams_events_before_a_blocking_tool_completes() {
    // Regression for #7 that actually tests INCREMENTAL streaming (not just
    // final ordering): a tool blocks on a signal file that only this test
    // creates. The `tool.call` line must reach stdout WHILE the tool is still
    // blocked — a buffered implementation writes nothing until the whole turn
    // (including the blocked shell) finishes, so it would deadlock here and the
    // recv would time out.
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let root = tempfile::tempdir().expect("root dir");
    let log = root.path().join("events.jsonl");
    let signal = root.path().join("unblock");
    let command = format!(
        "while [ ! -f '{}' ]; do sleep 0.02; done",
        signal.to_str().expect("signal path utf8")
    );
    let script = write_fixture_script(
        root.path(),
        "stream-block.json",
        &format!(
            r#"{{
  "version": 1,
  "responses": [
    {{ "events": [ {{ "tool_call": {{ "id": "c1", "name": "run_shell", "input": {{ "command": {} }} }} }}, {{ "finished": {{ "stop_reason": "tool_use" }} }} ] }},
    {{ "events": [ {{ "text_delta": "done" }}, {{ "finished": {{ "stop_reason": "completed" }} }} ] }}
  ]
}}"#,
            serde_json::to_string(&command).expect("encode command")
        ),
    );

    let mut child = command_with_home(exe, &home)
        .current_dir(root.path())
        .arg("exec")
        .arg("--provider")
        .arg("fixture")
        .arg("--provider-option")
        .arg(format!("event-script={}", path_str(&script)))
        .arg("--provenance")
        .arg(path_str(&log))
        .arg("--auto-approve")
        .arg("trusted-local")
        .arg("run it")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    let rx = spawn_line_reader(child.stdout.take().expect("stdout"));

    let mut streamed = false;
    loop {
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(line) if line.contains("tool.call: run_shell") => {
                streamed = true;
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    // Always unblock the shell so the process can exit, even on failure.
    fs::write(&signal, b"go").expect("write signal");
    let output = child.wait_with_output().expect("wait for euler");

    assert!(
        streamed,
        "tool.call must reach stdout before the blocked tool completes (streaming, not buffered)"
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn diagnostics_canary_excludes_user_tool_payloads_and_secret_sentinels() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let root = tempfile::tempdir().expect("root dir");
    let log = root.path().join("events.jsonl");
    let user_text = "DIAG_USER_TEXT_SENTINEL_20260704";
    let tool_payload = "DIAG_TOOL_PAYLOAD_SENTINEL_20260704";
    let fake_secret = "sk-diagnostics-fake-secret-20260704";
    let command = format!("printf '%s\\n' '{tool_payload} {fake_secret}'");
    let script = write_fixture_script(
        root.path(),
        "diagnostics-canary.json",
        &format!(
            r#"{{
  "version": 1,
  "responses": [
    {{
      "events": [
        {{
          "tool_call": {{
            "id": "call-shell",
            "name": "run_shell",
            "input": {{ "command": {} }}
          }}
        }},
        {{ "finished": {{ "stop_reason": "tool_use" }} }}
      ]
    }},
    {{
      "events": [
        {{ "text_delta": "done" }},
        {{ "finished": {{ "stop_reason": "completed" }} }}
      ]
    }}
  ]
}}
"#,
            serde_json::to_string(&command).expect("command json")
        ),
    );

    let output = command_with_home(exe, &home)
        .current_dir(root.path())
        .arg("exec")
        .arg("--provider")
        .arg("fixture")
        .arg("--provider-option")
        .arg(format!("event-script={}", path_str(&script)))
        .arg("--provenance")
        .arg(path_str(&log))
        .arg("--auto-approve")
        .arg("trusted-local")
        .arg(user_text)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run exec canary");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let diagnostics =
        fs::read_to_string(root.path().join("diagnostics.jsonl")).expect("read diagnostics");
    assert_no_forbidden_text(
        "diagnostics",
        &diagnostics,
        &[user_text, tool_payload, fake_secret],
    );
}

#[test]
fn fixture_loop_without_provenance_writes_home_session_store() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let mut child = command_with_home(exe, &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"hello home session\n")
        .expect("write stdin");

    let output = child.wait_with_output().expect("wait for euler");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout utf8"),
        "user: hello home session\nmodel.call: fixture/echo\nmodel.result: user: hello home session\nassistant: user: hello home session\n"
    );

    let sessions = home.path().join(".euler").join("sessions");
    let index = fs::read_to_string(sessions.join("index.jsonl")).expect("index");
    let entries = index.lines().collect::<Vec<_>>();
    assert_eq!(entries.len(), 2);
    let entry: serde_json::Value = serde_json::from_str(entries[0]).expect("index json");
    assert_eq!(
        entry.get("op").and_then(serde_json::Value::as_str),
        Some("created")
    );
    assert_eq!(
        entry
            .get("updated_at_ms")
            .and_then(serde_json::Value::as_u64),
        entry
            .get("created_at_ms")
            .and_then(serde_json::Value::as_u64)
    );
    let update: serde_json::Value = serde_json::from_str(entries[1]).expect("updated index json");
    assert_eq!(
        update.get("op").and_then(serde_json::Value::as_str),
        Some("updated")
    );
    let session_id = entry
        .get("id")
        .and_then(serde_json::Value::as_str)
        .expect("session id");
    assert_eq!(
        update.get("id").and_then(serde_json::Value::as_str),
        Some(session_id)
    );
    assert!(
        update
            .get("updated_at_ms")
            .and_then(serde_json::Value::as_u64)
            >= entry
                .get("updated_at_ms")
                .and_then(serde_json::Value::as_u64)
    );

    let log = sessions.join(session_id).join("events.jsonl");
    let events = read_jsonl(&log);
    assert_eq!(events.len(), 7);
    assert!(events.iter().all(|event| event.session == session_id));
    assert_eq!(events[0].kind.as_str(), EventKind::SESSION_START);
    assert!(sessions.join(session_id).join("session.json").is_file());
    assert!(sessions.join(session_id).join("blobs").is_dir());
}

#[test]
fn models_command_prints_merged_catalog_with_isolated_home() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let euler_home = home.path().join(".euler");
    fs::create_dir_all(&euler_home).expect("mkdir euler home");
    fs::write(
        euler_home.join("models.json"),
        r#"{
          "version": 1,
          "providers": {
            "missing": { "default_model": "ignored" },
            "chatgpt": {
              "default_model": "gpt-local-default",
              "models": [
                {
                  "id": "gpt-local-default",
                  "display_name": "GPT Local Default",
                  "context_window_tokens": 128000,
                  "max_output_tokens": "8192",
                  "supports_tools": true,
                  "supports_reasoning": "true",
                  "token": "SHOULD_NOT_APPEAR"
                }
              ]
            }
          }
        }"#,
    )
    .expect("write models config");

    let output = command_with_home(exe, &home)
        .arg("models")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run models");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(output.stderr).expect("stderr utf8");
    let catalog: serde_json::Value = serde_json::from_str(&stdout).expect("catalog json");
    let chatgpt = catalog["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .find(|provider| provider["id"] == "chatgpt")
        .expect("chatgpt provider");

    assert_eq!(chatgpt["default_model"], "gpt-local-default");
    let local_model = chatgpt["models"]
        .as_array()
        .expect("models")
        .iter()
        .find(|model| model["id"] == "gpt-local-default")
        .expect("local model");
    assert_eq!(local_model["display_name"], "GPT Local Default");
    assert_eq!(local_model["default"], true);
    assert_eq!(local_model["context_window_tokens"], 128000);
    assert_eq!(local_model["supports_tools"], true);
    assert!(local_model.get("max_output_tokens").is_none());
    assert!(local_model.get("supports_reasoning").is_none());
    assert!(stderr.contains("unknown provider `missing`"));
    assert!(stderr.contains("max_output_tokens"));
    assert!(stderr.contains("positive JSON integer"));
    assert!(stderr.contains("supports_reasoning"));
    assert!(stderr.contains("boolean"));
    assert!(!stdout.contains("unknown provider"));
    assert!(!stdout.contains("positive JSON integer"));
    assert!(!stdout.contains("SHOULD_NOT_APPEAR"));
    assert!(!stderr.contains("SHOULD_NOT_APPEAR"));
    assert!(
        !home.path().join(".euler").join("sessions").exists(),
        "models command should not create a session"
    );
}

#[test]
fn headless_exec_and_offline_listing_do_not_start_catalog_refresh() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let exec = command_with_home(exe, &home)
        .args([
            "exec",
            "--provider",
            "fixture",
            "--extensions",
            "none",
            "offline catalog check",
        ])
        .output()
        .expect("run exec");
    assert!(
        exec.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&exec.stderr)
    );
    let listed = command_with_home(exe, &home)
        .arg("models")
        .output()
        .expect("list models");
    assert!(listed.status.success());
    assert!(
        !home.path().join(".euler").join("catalogs").exists(),
        "headless commands must not create catalog refresh state"
    );
}

#[test]
fn models_command_without_local_catalog_prints_built_ins_without_session_store() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let output = command_with_home(exe, &home)
        .arg("models")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run models");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(output.stderr).expect("stderr utf8");
    let catalog: serde_json::Value = serde_json::from_str(&stdout).expect("catalog json");
    let provider_ids = catalog["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .map(|provider| provider["id"].as_str().expect("id").to_owned())
        .collect::<Vec<_>>();
    let embedded_manifest: serde_json::Value =
        serde_json::from_str(euler_provider::catalog::EMBEDDED_MANIFEST_JSON)
            .expect("embedded manifest json");

    assert_eq!(catalog["official_catalog"]["source"], "embedded");
    assert_eq!(
        catalog["official_catalog"]["release_id"],
        embedded_manifest["release_id"]
    );
    let openrouter = catalog["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .find(|provider| provider["id"] == "openrouter")
        .expect("OpenRouter provider");
    assert!(openrouter["models"]
        .as_array()
        .expect("OpenRouter models")
        .iter()
        .any(|model| model["id"] == "moonshotai/kimi-k3"));

    assert_eq!(
        provider_ids,
        vec![
            "anthropic",
            "chatgpt",
            "fixture",
            "openai",
            "openrouter",
            "xai"
        ]
    );
    assert!(stderr.is_empty(), "unexpected stderr: {stderr}");
    assert!(
        !home.path().join(".euler").join("sessions").exists(),
        "models command should not create a session"
    );
}

#[test]
fn failed_home_session_turn_refreshes_status_to_failed() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let script_dir = tempfile::tempdir().expect("script dir");
    let script = script_dir.path().join("one-response.json");
    fs::write(
        &script,
        r#"{
  "version": 1,
  "responses": [
    {
      "events": [
        { "text_delta": "first ok" },
        { "finished": { "stop_reason": "completed" } }
      ]
    }
  ]
}
"#,
    )
    .expect("write script");

    let mut child = command_with_home(exe, &home)
        .arg("--provider")
        .arg("fixture")
        .arg("--provider-option")
        .arg(format!("event-script={}", path_str(&script)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"first turn\nsecond turn\n")
        .expect("write stdin");

    let output = child.wait_with_output().expect("wait for euler");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("scripted provider exhausted"));

    let session_id = only_home_session_id(home.path());
    let metadata_path = home
        .path()
        .join(".euler")
        .join("sessions")
        .join(&session_id)
        .join("session.json");
    let metadata: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(metadata_path).expect("metadata"))
            .expect("metadata json");
    assert_eq!(
        metadata.get("status").and_then(serde_json::Value::as_str),
        Some("failed")
    );
    assert_eq!(
        home_index_ops(home.path()),
        vec!["created", "updated", "updated"]
    );
    let events = read_jsonl(&home_session_log(home.path(), &session_id));
    assert!(events
        .iter()
        .any(|event| event.kind.as_str() == EventKind::ERROR));
}

#[test]
fn headless_edit_file_persists_metadata_only_file_change_event() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let root = tempfile::tempdir().expect("root dir");
    let log = root.path().join("events.jsonl");
    let script_dir = tempfile::tempdir().expect("script dir");
    let script = script_dir.path().join("edit-file.json");
    fs::write(
        &script,
        r#"{
  "version": 1,
  "responses": [
    {
      "events": [
        {
          "tool_call": {
            "id": "call-create",
            "name": "edit_file",
            "input": {
              "path": "headless-created.txt",
              "old": "",
              "new": "hello\n"
            }
          }
        },
        { "finished": { "stop_reason": "tool_use" } }
      ]
    },
    {
      "events": [
        { "text_delta": "done" },
        { "finished": { "stop_reason": "completed" } }
      ]
    }
  ]
}
"#,
    )
    .expect("write script");

    let mut child = command_with_home(exe, &home)
        .current_dir(root.path())
        .arg("--provider")
        .arg("fixture")
        .arg("--provider-option")
        .arg(format!("event-script={}", path_str(&script)))
        .arg("--provenance")
        .arg(path_str(&log))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"create file\ny\n")
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait euler");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.path().join("headless-created.txt")).expect("created file"),
        "hello\n"
    );

    let events = read_jsonl(&log);
    let patch_applied = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::PATCH_APPLIED)
        .expect("patch applied");
    let file_change = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::FILE_CHANGE)
        .expect("file change");

    assert_eq!(
        file_change.parent.as_deref(),
        Some(patch_applied.id.as_str())
    );
    assert_eq!(
        file_change.payload.get("tool_call_id"),
        Some(&serde_json::json!("call-create"))
    );
    assert_eq!(
        file_change.payload.get("origin"),
        Some(&serde_json::json!("edit_file"))
    );
    assert_eq!(
        file_change.payload.get("action"),
        Some(&serde_json::json!("add"))
    );
    assert_eq!(
        file_change.payload.get("path"),
        Some(&serde_json::json!("headless-created.txt"))
    );
    assert_eq!(
        file_change.payload.get("before_sha256"),
        Some(&serde_json::Value::Null)
    );
    assert_eq!(
        file_change.payload.get("before_byte_len"),
        Some(&serde_json::json!(0))
    );
    assert_eq!(
        file_change.payload.get("after_byte_len"),
        Some(&serde_json::json!(6))
    );
    assert_eq!(
        file_change.payload.get("diff_redaction"),
        Some(&serde_json::json!("omitted"))
    );
    assert!(!file_change.payload.contains_key("old"));
    assert!(!file_change.payload.contains_key("new"));
    assert!(!file_change.payload.contains_key("diff"));
    let file_change_json = file_change.to_json_line().expect("serialize file.change");
    assert!(!file_change_json.contains("hello"));

    let resumed = run_euler_with_input_from(exe, &["--resume", path_str(&log)], "", root.path());
    assert!(
        resumed.status.success(),
        "resume stderr: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
}

#[test]
fn headless_write_file_persists_metadata_only_file_change_event() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let root = tempfile::tempdir().expect("root dir");
    let log = root.path().join("events.jsonl");
    let script_dir = tempfile::tempdir().expect("script dir");
    let script = script_dir.path().join("write-file.json");
    fs::write(
        &script,
        r#"{
  "version": 1,
  "responses": [
    {
      "events": [
        {
          "tool_call": {
            "id": "call-write",
            "name": "write_file",
            "input": {
              "path": "headless-written.txt",
              "content": "hello\n"
            }
          }
        },
        { "finished": { "stop_reason": "tool_use" } }
      ]
    },
    {
      "events": [
        { "text_delta": "done" },
        { "finished": { "stop_reason": "completed" } }
      ]
    }
  ]
}
"#,
    )
    .expect("write script");

    let mut child = command_with_home(exe, &home)
        .current_dir(root.path())
        .arg("--provider")
        .arg("fixture")
        .arg("--provider-option")
        .arg(format!("event-script={}", path_str(&script)))
        .arg("--provenance")
        .arg(path_str(&log))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"create file\ny\n")
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait euler");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.path().join("headless-written.txt")).expect("created file"),
        "hello\n"
    );

    let events = read_jsonl(&log);
    let prompt = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::PERMISSION_PROMPT)
        .expect("permission prompt");
    let patch_applied = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::PATCH_APPLIED)
        .expect("patch applied");
    let file_change = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::FILE_CHANGE)
        .expect("file change");

    assert_eq!(
        prompt.payload.get("reason"),
        Some(&serde_json::json!("tool write_file"))
    );
    assert_eq!(
        file_change.parent.as_deref(),
        Some(patch_applied.id.as_str())
    );
    assert_eq!(
        file_change.payload.get("tool_call_id"),
        Some(&serde_json::json!("call-write"))
    );
    assert_eq!(
        file_change.payload.get("origin"),
        Some(&serde_json::json!("write_file"))
    );
    assert_eq!(
        file_change.payload.get("action"),
        Some(&serde_json::json!("add"))
    );
    assert_eq!(
        file_change.payload.get("path"),
        Some(&serde_json::json!("headless-written.txt"))
    );
    assert_eq!(
        file_change.payload.get("before_sha256"),
        Some(&serde_json::Value::Null)
    );
    assert_eq!(
        file_change.payload.get("before_byte_len"),
        Some(&serde_json::json!(0))
    );
    assert_eq!(
        file_change.payload.get("after_byte_len"),
        Some(&serde_json::json!(6))
    );
    assert_eq!(
        file_change.payload.get("diff_redaction"),
        Some(&serde_json::json!("omitted"))
    );
    assert!(!file_change.payload.contains_key("old"));
    assert!(!file_change.payload.contains_key("new"));
    assert!(!file_change.payload.contains_key("diff"));
    let file_change_json = file_change.to_json_line().expect("serialize file.change");
    assert!(!file_change_json.contains("hello"));

    let resumed = run_euler_with_input_from(exe, &["--resume", path_str(&log)], "", root.path());
    assert!(
        resumed.status.success(),
        "resume stderr: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
}

#[test]
fn headless_direct_apply_patch_persists_metadata_only_file_change_event() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let root = tempfile::tempdir().expect("root dir");
    let log = root.path().join("events.jsonl");
    let script_dir = tempfile::tempdir().expect("script dir");
    let script = script_dir.path().join("apply-patch-direct.json");
    let patch = "*** Begin Patch\n*** Add File: direct-created.txt\n+hello\n*** End Patch";
    fs::write(
        &script,
        serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            "responses": [
                {
                    "events": [
                        {
                            "tool_call": {
                                "id": "call-direct-apply",
                                "name": "apply_patch",
                                "input": {
                                    "patch": patch
                                }
                            }
                        },
                        { "finished": { "stop_reason": "tool_use" } }
                    ]
                },
                {
                    "events": [
                        { "text_delta": "done" },
                        { "finished": { "stop_reason": "completed" } }
                    ]
                }
            ]
        }))
        .expect("script json"),
    )
    .expect("write script");

    let mut child = command_with_home(exe, &home)
        .current_dir(root.path())
        .arg("--provider")
        .arg("fixture")
        .arg("--provider-option")
        .arg(format!("event-script={}", path_str(&script)))
        .arg("--provenance")
        .arg(path_str(&log))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"create file\ny\n")
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait euler");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.path().join("direct-created.txt")).expect("created file"),
        "hello\n"
    );

    let events = read_jsonl(&log);
    let permission_prompt = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::PERMISSION_PROMPT)
        .expect("permission prompt");
    let file_change = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::FILE_CHANGE)
        .expect("file change");
    let tool_result = events
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::TOOL_RESULT
                && event.payload.get("id") == Some(&serde_json::json!("call-direct-apply"))
        })
        .expect("tool result");

    assert_eq!(
        permission_prompt.payload.get("capability"),
        Some(&serde_json::json!("fs-write"))
    );
    assert_eq!(
        permission_prompt.payload.get("reason"),
        Some(&serde_json::json!("tool apply_patch"))
    );
    assert_eq!(
        file_change.payload.get("tool_call_id"),
        Some(&serde_json::json!("call-direct-apply"))
    );
    assert_eq!(
        file_change.payload.get("origin"),
        Some(&serde_json::json!("apply_patch"))
    );
    assert_eq!(
        file_change.payload.get("action"),
        Some(&serde_json::json!("add"))
    );
    assert_eq!(
        tool_result
            .payload
            .get("output")
            .and_then(serde_json::Value::as_str),
        Some("apply_patch prepared add direct-created.txt")
    );
    assert!(!file_change.payload.contains_key("old"));
    assert!(!file_change.payload.contains_key("new"));
    assert!(!file_change.payload.contains_key("diff"));
    let file_change_json = file_change.to_json_line().expect("serialize file.change");
    assert!(!file_change_json.contains("hello"));
    assert!(!file_change_json.contains("*** Begin Patch"));

    let resumed = run_euler_with_input_from(exe, &["--resume", path_str(&log)], "", root.path());
    assert!(
        resumed.status.success(),
        "resume stderr: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
}

#[test]
fn headless_apply_patch_shell_intercept_persists_metadata_only_file_change_event() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let root = tempfile::tempdir().expect("root dir");
    let log = root.path().join("events.jsonl");
    let script_dir = tempfile::tempdir().expect("script dir");
    let script = script_dir.path().join("apply-patch-shell.json");
    let command =
        "apply_patch <<'PATCH'\n*** Begin Patch\n*** Add File: shell-created.txt\n+hello\n*** End Patch\nPATCH";
    fs::write(
        &script,
        serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            "responses": [
                {
                    "events": [
                        {
                            "tool_call": {
                                "id": "call-shell-apply",
                                "name": "run_shell",
                                "input": {
                                    "command": command
                                }
                            }
                        },
                        { "finished": { "stop_reason": "tool_use" } }
                    ]
                },
                {
                    "events": [
                        { "text_delta": "done" },
                        { "finished": { "stop_reason": "completed" } }
                    ]
                }
            ]
        }))
        .expect("script json"),
    )
    .expect("write script");

    let mut child = command_with_home(exe, &home)
        .current_dir(root.path())
        .arg("--provider")
        .arg("fixture")
        .arg("--provider-option")
        .arg(format!("event-script={}", path_str(&script)))
        .arg("--provenance")
        .arg(path_str(&log))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"create file\ny\n")
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait euler");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.path().join("shell-created.txt")).expect("created file"),
        "hello\n"
    );

    let events = read_jsonl(&log);
    let permission_prompt = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::PERMISSION_PROMPT)
        .expect("permission prompt");
    let file_change = events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::FILE_CHANGE)
        .expect("file change");
    let tool_result = events
        .iter()
        .find(|event| {
            event.kind.as_str() == EventKind::TOOL_RESULT
                && event.payload.get("id") == Some(&serde_json::json!("call-shell-apply"))
        })
        .expect("tool result");

    assert_eq!(
        permission_prompt.payload.get("capability"),
        Some(&serde_json::json!("fs-write"))
    );
    assert_eq!(
        permission_prompt.payload.get("reason"),
        Some(&serde_json::json!("tool apply_patch"))
    );
    assert_eq!(
        file_change.payload.get("tool_call_id"),
        Some(&serde_json::json!("call-shell-apply"))
    );
    assert_eq!(
        file_change.payload.get("origin"),
        Some(&serde_json::json!("run_shell:apply_patch"))
    );
    assert_eq!(
        file_change.payload.get("action"),
        Some(&serde_json::json!("add"))
    );
    assert_eq!(
        tool_result
            .payload
            .get("output")
            .and_then(serde_json::Value::as_str),
        Some("intercepted apply_patch prepared add shell-created.txt")
    );
    assert!(!file_change.payload.contains_key("old"));
    assert!(!file_change.payload.contains_key("new"));
    assert!(!file_change.payload.contains_key("diff"));
    let file_change_json = file_change.to_json_line().expect("serialize file.change");
    assert!(!file_change_json.contains("hello"));
    assert!(!file_change_json.contains("*** Begin Patch"));

    let resumed = run_euler_with_input_from(exe, &["--resume", path_str(&log)], "", root.path());
    assert!(
        resumed.status.success(),
        "resume stderr: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
}

#[test]
fn secret_env_values_do_not_enter_home_session_persistence() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let secrets = SecretFixture::new(home.path());
    let input = "home persistence control text\n";
    assert_fixture_input_is_secret_free(input, &secrets.sentinels);

    let mut child = command_with_secret_env(exe, &home, &secrets)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("wait for euler");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("home persistence control text"));

    let artifacts = collect_home_session_artifacts(home.path(), "home persistence control text");
    for path in artifacts {
        assert_no_sentinels_in_file(&path, &secrets.sentinels);
    }
}

#[test]
fn secret_env_values_do_not_enter_explicit_provenance_log() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let temp = tempfile::tempdir().expect("temp dir");
    let home = isolated_home();
    let secrets = SecretFixture::new(home.path());
    let provenance = temp.path().join("explicit-provenance.jsonl");
    let input = "explicit provenance control text\n";
    assert_fixture_input_is_secret_free(input, &secrets.sentinels);

    let mut child = command_with_secret_env(exe, &home, &secrets)
        .arg("--provenance")
        .arg(&provenance)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("wait for euler");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("explicit provenance control text"));
    assert_nonempty_file(&provenance);

    let provenance_text = fs::read_to_string(&provenance).expect("read provenance");
    assert!(provenance_text.contains("explicit provenance control text"));
    assert_no_sentinels_in_file(&provenance, &secrets.sentinels);
}

#[test]
fn resume_by_home_session_id_appends_to_saved_session() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let mut first = command_with_home(exe, &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn first euler");
    first
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"alpha\n")
        .expect("write first stdin");
    let first = first.wait_with_output().expect("wait first");
    assert!(first.status.success());

    let session_id = only_home_session_id(home.path());
    let resumed = command_with_home(exe, &home)
        .arg("--resume")
        .arg(&session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.as_mut().expect("stdin").write_all(b"beta\n")?;
            child.wait_with_output()
        })
        .expect("run resume");

    assert!(resumed.status.success());
    let stderr = String::from_utf8_lossy(&resumed.stderr);
    assert!(stderr.contains(&format!("resumed session {session_id}")));
    let log = home_session_log(home.path(), &session_id);
    let events = read_jsonl(&log);
    assert!(events.iter().all(|event| event.session == session_id));
    let replayed = replay_transcript_with_home(exe, home.path(), &log);
    assert!(replayed.contains("user: alpha\n"));
    assert!(replayed.contains("user: beta\n"));
    assert_eq!(
        home_index_ops(home.path()),
        vec!["created", "updated", "updated"]
    );
}

#[test]
fn resume_by_home_session_name_appends_to_saved_session() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let mut first = command_with_home(exe, &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn first euler");
    first
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"alpha named\n")
        .expect("write first stdin");
    assert!(first
        .wait_with_output()
        .expect("wait first")
        .status
        .success());

    let session_id = only_home_session_id(home.path());
    let log = home_session_log(home.path(), &session_id);
    append_session_rename_event(&log, &session_id, "dogfood session");

    let resumed = command_with_home(exe, &home)
        .arg("--resume")
        .arg("dogfood session")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(b"beta named\n")?;
            child.wait_with_output()
        })
        .expect("run resume");

    assert!(resumed.status.success());
    let stderr = String::from_utf8_lossy(&resumed.stderr);
    assert!(stderr.contains(&format!("resumed session {session_id}")));
    let events = read_jsonl(&log);
    assert!(events.iter().all(|event| event.session == session_id));
    assert!(events.iter().any(|event| {
        event.kind.as_str() == EventKind::SESSION_RENAMED
            && event
                .payload
                .get("name")
                .and_then(serde_json::Value::as_str)
                == Some("dogfood session")
    }));
    let replayed = replay_transcript_with_home(exe, home.path(), &log);
    assert!(replayed.contains("user: alpha named\n"));
    assert!(replayed.contains("user: beta named\n"));
    assert_eq!(
        home_index_ops(home.path()),
        vec!["created", "updated", "updated"]
    );
}

#[test]
fn headless_extension_run_error_line_does_not_end_session() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let root = tempfile::tempdir().expect("root dir");
    let log = root.path().join("events.jsonl");

    let mut child = command_with_home(exe, &home)
        .current_dir(root.path())
        .arg("--provenance")
        .arg(path_str(&log))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(
            b"before error\nextension_run session-export.session-export {\"unknown\":true}\nafter error\n",
        )
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait euler");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let error_line = stdout
        .lines()
        .find(|line| line.starts_with('{'))
        .expect("extension error JSON line");
    let error_json: serde_json::Value = serde_json::from_str(error_line).expect("error json");
    assert_eq!(error_json["type"], serde_json::json!("error"));
    assert_eq!(error_json["source"], serde_json::json!("extension_run"));
    assert!(stdout.contains("user: after error"));
    // An unknown extension id never reaches the session, so no error event is
    // persisted; the session simply continues to the next turn.
    let events = read_jsonl(&log);
    assert!(events.iter().any(|event| {
        event.kind.as_str() == EventKind::USER_MESSAGE
            && event.payload.get("content") == Some(&serde_json::json!("after error"))
    }));
}

#[test]
fn headless_extension_run_malformed_requests_error_without_ending_session() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let root = tempfile::tempdir().expect("root dir");
    let log = root.path().join("events.jsonl");

    let mut child = command_with_home(exe, &home)
        .current_dir(root.path())
        .arg("--provenance")
        .arg(path_str(&log))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(
            b"extension_run \nextension_run session-export\nextension_run session-export.session-export\nextension_run session-export.session-export {\nextension_run nope.export {}\nstill alive\n",
        )
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait euler");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let error_lines = stdout
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("error json"))
        .collect::<Vec<_>>();
    assert_eq!(
        error_lines.len(),
        5,
        "every malformed request yields exactly one error line: {stdout}"
    );
    for error_json in &error_lines {
        assert_eq!(error_json["type"], serde_json::json!("error"));
        assert_eq!(error_json["source"], serde_json::json!("extension_run"));
    }
    assert!(stdout.contains("user: still alive"));
}

#[test]
fn headless_companion_run_malformed_request_errors_without_ending_session() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let root = tempfile::tempdir().expect("root dir");
    let log = root.path().join("events.jsonl");

    let mut child = command_with_home(exe, &home)
        .current_dir(root.path())
        .arg("--provenance")
        .arg(path_str(&log))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"companion_run {not json\ncompanion_run\nstill alive\n")
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait euler");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let error_lines = stdout
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("error json"))
        .collect::<Vec<_>>();
    assert_eq!(
        error_lines.len(),
        2,
        "each malformed companion_run yields one error line: {stdout}"
    );
    for error_json in &error_lines {
        assert_eq!(error_json["type"], serde_json::json!("error"));
        assert_eq!(error_json["source"], serde_json::json!("companion_run"));
    }
    assert!(stdout.contains("user: still alive"));
}

#[test]
fn headless_extension_run_waits_for_active_turn() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let root = tempfile::tempdir().expect("root dir");
    let log = root.path().join("events.jsonl");
    let script = write_fixture_script(
        root.path(),
        "slow-script.json",
        r#"{
  "version": 1,
  "responses": [
    {
      "events": [
        { "sleep_ms": 300 },
        { "text_delta": "slow done" },
        { "finished": { "stop_reason": "completed" } }
      ]
    }
  ]
}
"#,
    );

    let mut child = command_with_home(exe, &home)
        .current_dir(root.path())
        .arg("--provider")
        .arg("fixture")
        .arg("--provider-option")
        .arg(format!("event-script={}", path_str(&script)))
        .arg("--provenance")
        .arg(path_str(&log))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"slow turn\nextension_run session-export.session-export {}\n")
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait euler");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let extension_line = stdout
        .lines()
        .find(|line| line.starts_with('{'))
        .expect("extension result JSON line");
    assert!(
        stdout.find("assistant: slow done").expect("turn output")
            < stdout.find(extension_line).expect("extension output")
    );
}

#[test]
fn headless_extension_run_then_closed_stdin_exits_cleanly() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let root = tempfile::tempdir().expect("root dir");
    let log = root.path().join("events.jsonl");

    let mut child = command_with_home(exe, &home)
        .current_dir(root.path())
        .arg("--provenance")
        .arg(path_str(&log))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"extension_run session-export.session-export {}\n")
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait euler");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(stdout.lines().any(|line| line.starts_with('{')));
}

#[test]
fn extension_resolution_rejects_unknown_ids_and_malformed_project_file() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let valid = "no extensions are linked or installed; add one with `euler extension link` or `euler extension install`";

    let cli_home = isolated_home();
    let cli_root = tempfile::tempdir().expect("cli root");
    let cli = command_with_home(exe, &cli_home)
        .current_dir(cli_root.path())
        .args(["--extensions", "nope"])
        .stderr(Stdio::piped())
        .output()
        .expect("cli unknown");
    assert!(!cli.status.success());
    assert!(String::from_utf8_lossy(&cli.stderr)
        .contains(&format!("unknown extension id: nope; {valid}")));

    let project_home = isolated_home();
    let project_root = tempfile::tempdir().expect("project root");
    // Canonicalize: the binary reports the project file via its resolved cwd,
    // and on macOS `TempDir::path()` is the `/var/…` symlink form of
    // `/private/var/…`.
    let project_root_path = project_root
        .path()
        .canonicalize()
        .expect("canonical project root");
    let project_dir = project_root_path.join(".euler");
    fs::create_dir(&project_dir).expect("project euler dir");
    let project_file = project_dir.join("extensions.json");
    fs::write(&project_file, r#"{"enable":["nope"]}"#).expect("project overlay");
    let project = command_with_home(exe, &project_home)
        .current_dir(project_root.path())
        .stderr(Stdio::piped())
        .output()
        .expect("project unknown");
    assert!(!project.status.success());
    let project_stderr = String::from_utf8_lossy(&project.stderr);
    // The binary reports the canonicalized overlay path (macOS tempdirs live
    // behind the /var -> /private/var symlink).
    let canonical_project_file = project_file
        .canonicalize()
        .expect("canonicalize project overlay path");
    assert!(project_stderr.contains(&format!(
        "unknown extension id in {}: nope; {valid}",
        canonical_project_file.display()
    )));

    let malformed_home = isolated_home();
    let malformed_root = tempfile::tempdir().expect("malformed root");
    let malformed_root_path = malformed_root
        .path()
        .canonicalize()
        .expect("canonical malformed root");
    let malformed_dir = malformed_root_path.join(".euler");
    fs::create_dir(&malformed_dir).expect("malformed euler dir");
    let malformed_file = malformed_dir.join("extensions.json");
    fs::write(&malformed_file, "{").expect("malformed overlay");
    let malformed = command_with_home(exe, &malformed_home)
        .current_dir(malformed_root.path())
        .stderr(Stdio::piped())
        .output()
        .expect("malformed project");
    assert!(!malformed.status.success());
    let canonical_malformed_file = malformed_file
        .canonicalize()
        .expect("canonicalize malformed overlay path");
    assert!(
        String::from_utf8_lossy(&malformed.stderr).contains(&format!(
            "malformed project extensions file {}",
            canonical_malformed_file.display()
        ))
    );

    // A registry enablement entry for an id that is no longer present (e.g. a
    // formerly bundled extension) is stale state, not corruption: the session
    // starts and simply skips it.
    let registry_home = isolated_home();
    let registry_root = tempfile::tempdir().expect("registry root");
    write_registry_state(&registry_home, "nope", "enable");
    let registry = command_with_home(exe, &registry_home)
        .current_dir(registry_root.path())
        .stderr(Stdio::piped())
        .output()
        .expect("registry stale id");
    assert!(
        registry.status.success(),
        "stale registry id must not fail the session: {}",
        String::from_utf8_lossy(&registry.stderr)
    );
    assert!(!String::from_utf8_lossy(&registry.stderr).contains("unknown extension id"));
}

#[test]
fn extensions_flag_rejects_empty_and_duplicate_values() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let root = tempfile::tempdir().expect("root dir");

    let missing = command_with_home(exe, &home)
        .current_dir(root.path())
        .arg("--extensions")
        .stderr(Stdio::piped())
        .output()
        .expect("missing value");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--extensions requires a value"));

    let empty = command_with_home(exe, &home)
        .current_dir(root.path())
        .arg("--extensions")
        .arg("")
        .stderr(Stdio::piped())
        .output()
        .expect("empty value");
    assert!(!empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stderr).contains("--extensions requires a value"));

    let duplicate = command_with_home(exe, &home)
        .current_dir(root.path())
        .args(["--extensions", "none", "--extensions", "none"])
        .stderr(Stdio::piped())
        .output()
        .expect("duplicate flag");
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr)
        .contains("--extensions was provided more than once"));
}

#[test]
fn extension_cli_corrupt_registry_fails_session_start_closed() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let registry_dir = home.path().join(".euler").join("extensions");
    fs::create_dir_all(&registry_dir).expect("registry dir");
    fs::write(registry_dir.join("state.jsonl"), "not json\n").expect("corrupt registry");

    // The listing surface reads the link inventory, not the enablement log:
    // with nothing linked it prints nothing and succeeds.
    let listed = command_with_home(exe, &home)
        .args(["extension", "list"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension list corrupt");
    assert!(listed.status.success());
    assert!(listed.stdout.is_empty());

    // Ids that are not linked are unknown, corruption or not.
    for args in [
        &["extension", "status", "session-export"][..],
        &["extension", "enable", "session-export"][..],
        &["extension", "disable", "session-export"][..],
    ] {
        let output = command_with_home(exe, &home)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("extension unknown id");
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unknown extension id"));
    }

    // Session-start resolution folds the enablement log and must fail closed
    // on corruption rather than silently dropping registry state.
    let root = tempfile::tempdir().expect("root dir");
    let session = command_with_home(exe, &home)
        .current_dir(root.path())
        .stderr(Stdio::piped())
        .output()
        .expect("session with corrupt registry");
    assert!(!session.status.success());
    assert!(String::from_utf8_lossy(&session.stderr).contains("invalid line"));
}

#[test]
fn extension_search_survives_unavailable_registry_dir() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let euler_dir = home.path().join(".euler");
    fs::create_dir_all(&euler_dir).expect("euler dir");
    fs::write(euler_dir.join("extensions"), "not a directory").expect("registry file");

    let output = command_with_home(exe, &home)
        .args(["extension", "search", "example"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension search registry unavailable");
    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "search stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).expect("search json");
    let results = parsed["results"].as_array().expect("results");
    assert!(results.is_empty());
}

#[test]
fn extension_search_ignores_corrupt_link_inventory_without_leaking_paths() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let registry_dir = home.path().join(".euler").join("extensions");
    fs::create_dir_all(&registry_dir).expect("registry dir");
    let secret_path = "/tmp/euler-search-SHOULD_NOT_APPEAR";
    let corrupt_inventory = serde_json::json!({
        "v": 1,
        "links": {
            "example-extension": {
                "source_path": secret_path,
                "manifest_sha256": "abc123-SHOULD_NOT_APPEAR",
                "updated_ts_ms": 7,
                "status": "needs-review",
                "broken_reason": null,
                "descriptor": {
                    "id": "different-extension",
                    "display_name": "Example Extension",
                    "version": "0.1.0",
                    "runtime_kind": "native-rust",
                    "capabilities": ["provenance-read"],
                    "commands": [{
                        "name": "inspect",
                        "display_name": "Inspect",
                        "summary": "Inspect provenance.",
                        "required_capabilities": ["provenance-read"]
                    }]
                }
            }
        }
    });
    fs::write(
        registry_dir.join("links.json"),
        serde_json::to_vec_pretty(&corrupt_inventory).expect("inventory json"),
    )
    .expect("corrupt link inventory");

    let output = command_with_home(exe, &home)
        .args(["extension", "search", "example"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension search corrupt links");
    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "search stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("search stdout utf8");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("search json");
    let results = parsed["results"].as_array().expect("results");
    assert!(results.is_empty(), "corrupt inventory entries are ignored");
    assert!(!stdout.contains(secret_path));
    assert!(!stdout.contains("SHOULD_NOT_APPEAR"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(secret_path));
}

#[test]
fn extension_search_reports_linked_metadata_without_private_inventory_fields() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let extension_dir = tempfile::tempdir().expect("extension dir");
    write_extension_manifest(extension_dir.path(), "example-extension", "0.1.0");

    let linked = command_with_home(exe, &home)
        .args(["extension", "link", path_str(extension_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension link");
    assert!(linked.status.success());
    assert!(linked.stderr.is_empty());
    fs::remove_file(
        extension_dir
            .path()
            .join(euler_core::EXTENSION_MANIFEST_FILE),
    )
    .expect("remove linked manifest after link");

    let output = command_with_home(exe, &home)
        .args(["extension", "search", "example"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension search linked");
    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "search stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("search stdout utf8");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("search json");
    assert_eq!(parsed["query"], "example");
    let results = parsed["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    let result = &results[0];
    assert_eq!(result["id"], "example-extension");
    assert_eq!(result["display_name"], "Example Extension");
    assert_eq!(result["version"], "0.1.0");
    assert_eq!(result["source_kind"], "linked");
    assert_eq!(result["runtime_kind"], "native-rust");
    assert_eq!(result["status"], "needs-review");
    assert_eq!(result["requires_review"], serde_json::json!(true));
    assert_eq!(result["requires_execution_grant"], serde_json::json!(false));
    assert_eq!(
        result["capabilities"],
        serde_json::json!(["provenance-read"])
    );
    assert_eq!(result["commands"][0]["name"], "inspect");
    assert_eq!(result["commands"][0]["display_name"], "Inspect");
    assert_eq!(result["commands"][0]["summary"], "Inspect provenance.");
    assert_eq!(
        result["commands"][0]["required_capabilities"],
        serde_json::json!(["provenance-read"])
    );
    assert!(!stdout.contains(extension_dir.path().to_string_lossy().as_ref()));
    assert!(!stdout.contains("source_path"));
    assert!(!stdout.contains("manifest_sha256"));
    assert!(!stdout.contains("updated_ts_ms"));
    assert!(!stdout.contains("broken_reason"));

    let mixed = command_with_home(exe, &home)
        .args(["extension", "search", "provenance"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension search mixed results");
    assert!(mixed.status.success());
    let mixed_json: serde_json::Value =
        serde_json::from_slice(&mixed.stdout).expect("mixed search json");
    let ids: Vec<&str> = mixed_json["results"]
        .as_array()
        .expect("mixed results")
        .iter()
        .map(|result| result["id"].as_str().expect("result id"))
        .collect();
    assert_eq!(ids, vec!["example-extension"]);
}

#[test]
fn extension_audit_reports_empty_and_healthy_registry_without_paths() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let empty = command_with_home(exe, &home)
        .args(["extension", "audit"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension audit empty");
    assert!(empty.status.success());
    assert!(empty.stderr.is_empty());
    let empty_json: serde_json::Value =
        serde_json::from_slice(&empty.stdout).expect("empty audit json");
    assert_eq!(empty_json["schema_version"], serde_json::json!(1));
    assert_eq!(
        empty_json["entries"]
            .as_array()
            .expect("empty audit entries")
            .len(),
        0
    );
    assert!(
        !home.path().join(".euler").join("extensions").exists(),
        "empty audit must not create the extension registry directory"
    );

    let linked_dir = tempfile::tempdir().expect("linked extension dir");
    write_extension_manifest(linked_dir.path(), "linked-extension", "0.1.0");
    let linked = command_with_home(exe, &home)
        .args(["extension", "link", path_str(linked_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension link");
    assert!(linked.status.success());

    let installed_dir = tempfile::tempdir().expect("installed extension dir");
    write_extension_manifest(installed_dir.path(), "installed-extension", "0.1.0");
    let installed = command_with_home(exe, &home)
        .args(["extension", "install", path_str(installed_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension install");
    assert!(installed.status.success());

    let inventory_path = home
        .path()
        .join(".euler")
        .join("extensions")
        .join("links.json");
    let inventory_before = fs::read(&inventory_path).expect("inventory before");
    let installed_root = home
        .path()
        .join(".euler")
        .join("extensions")
        .join("installed");

    let audit = command_with_home(exe, &home)
        .args(["extension", "audit"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension audit healthy");
    assert!(audit.status.success());
    assert!(
        audit.stderr.is_empty(),
        "audit stderr: {}",
        String::from_utf8_lossy(&audit.stderr)
    );
    assert_eq!(
        fs::read(&inventory_path).expect("inventory after"),
        inventory_before,
        "audit must not rewrite inventory"
    );
    assert!(installed_root.exists());

    let stdout = String::from_utf8(audit.stdout).expect("audit stdout utf8");
    assert_no_path_leak(
        &stdout,
        &[
            home.path(),
            linked_dir.path(),
            installed_dir.path(),
            &installed_root,
        ],
    );
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("audit json");
    assert_eq!(audit_issue_code(&parsed, "linked-extension"), "ok");
    assert_eq!(audit_issue_code(&parsed, "installed-extension"), "ok");
}

#[cfg(unix)]
#[test]
fn extension_audit_rejects_symlinked_registry_root_without_path_leaks() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let target = tempfile::tempdir().expect("registry target");
    let euler_home = home.path().join(".euler");
    fs::create_dir_all(&euler_home).expect("euler home");
    symlink(target.path(), euler_home.join("extensions")).expect("registry root symlink");

    let audit = command_with_home(exe, &home)
        .args(["extension", "audit"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension audit");

    assert!(!audit.status.success());
    let stdout = String::from_utf8(audit.stdout).expect("audit stdout utf8");
    let stderr = String::from_utf8(audit.stderr).expect("audit stderr utf8");
    assert_no_path_leak(&stdout, &[target.path()]);
    assert_no_path_leak(&stderr, &[target.path()]);
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("audit json");
    assert_eq!(
        report,
        serde_json::json!({
            "schema_version": 1,
            "error": {
                "code": "registry-unavailable",
                "message": "extension registry audit failed"
            }
        })
    );
    assert_eq!(stderr, "Error: extension audit failed\n");
    assert!(target
        .path()
        .read_dir()
        .expect("target entries")
        .next()
        .is_none());
}

#[test]
fn extension_audit_reports_drift_and_corrupt_inventory_safely() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let extension_dir = tempfile::tempdir().expect("extension dir");
    write_extension_manifest(extension_dir.path(), "example-extension", "0.1.0");

    let linked = command_with_home(exe, &home)
        .args(["extension", "link", path_str(extension_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension link");
    assert!(linked.status.success());
    write_extension_manifest(extension_dir.path(), "renamed-extension", "0.1.0");

    let drift = command_with_home(exe, &home)
        .args(["extension", "audit"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension audit drift");
    assert!(drift.status.success());
    assert!(drift.stderr.is_empty());
    let drift_stdout = String::from_utf8(drift.stdout).expect("drift stdout utf8");
    assert_no_path_leak(&drift_stdout, &[home.path(), extension_dir.path()]);
    let drift_json: serde_json::Value = serde_json::from_str(&drift_stdout).expect("drift json");
    assert_eq!(
        audit_issue_code(&drift_json, "example-extension"),
        "linked-manifest-id-mismatch"
    );

    let inventory_path = home
        .path()
        .join(".euler")
        .join("extensions")
        .join("links.json");
    fs::write(&inventory_path, "not json\n").expect("corrupt inventory");
    let corrupt = command_with_home(exe, &home)
        .args(["extension", "audit"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension audit corrupt");
    assert!(!corrupt.status.success());
    let corrupt_stdout = String::from_utf8(corrupt.stdout).expect("corrupt stdout utf8");
    let corrupt_stderr = String::from_utf8(corrupt.stderr).expect("corrupt stderr utf8");
    assert_no_path_leak(&corrupt_stdout, &[home.path(), extension_dir.path()]);
    assert_no_path_leak(&corrupt_stderr, &[home.path(), extension_dir.path()]);
    let corrupt_json: serde_json::Value =
        serde_json::from_str(&corrupt_stdout).expect("corrupt audit json");
    assert_eq!(
        corrupt_json,
        serde_json::json!({
            "schema_version": 1,
            "error": {
                "code": "registry-inventory-invalid",
                "message": "extension registry audit failed"
            }
        })
    );
    assert_eq!(corrupt_stderr, "Error: extension audit failed\n");
}

#[test]
fn extension_install_registers_inert_metadata_without_source_path_leaks() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let extension_dir = tempfile::tempdir().expect("extension dir");
    write_extension_manifest(extension_dir.path(), "example-extension", "0.1.0");

    let installed = command_with_home(exe, &home)
        .args(["extension", "install", path_str(extension_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension install");
    assert!(installed.status.success());
    assert!(installed.stderr.is_empty());
    let installed_stdout = String::from_utf8(installed.stdout).expect("install stdout utf8");
    let installed_json: serde_json::Value =
        serde_json::from_str(&installed_stdout).expect("install json");
    assert_eq!(installed_json["id"], "example-extension");
    assert_eq!(installed_json["source_kind"], "installed");
    assert_eq!(installed_json["status"], "installed-inert");
    assert_eq!(
        installed_json["execution_granted"],
        serde_json::json!(false)
    );
    assert_eq!(
        installed_json["requires_execution_grant"],
        serde_json::json!(true)
    );
    assert!(!installed_stdout.contains(extension_dir.path().to_string_lossy().as_ref()));
    assert!(installed_json.get("source_path").is_none());

    let installed_root = home
        .path()
        .join(".euler")
        .join("extensions")
        .join("installed")
        .join("example-extension");
    assert!(installed_root.exists());
    assert!(!installed_stdout.contains(installed_root.to_string_lossy().as_ref()));

    let listed = command_with_home(exe, &home)
        .args(["extension", "list"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension list installed");
    assert!(listed.status.success());
    assert!(String::from_utf8(listed.stdout)
        .expect("list stdout")
        .contains("example-extension installed-inert installed"));

    let status = command_with_home(exe, &home)
        .args(["extension", "status", "example-extension"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension status installed");
    assert!(status.status.success());
    assert_eq!(
        String::from_utf8(status.stdout).expect("status stdout"),
        "example-extension installed-inert installed\n"
    );

    fs::remove_file(
        extension_dir
            .path()
            .join(euler_core::EXTENSION_MANIFEST_FILE),
    )
    .expect("remove source manifest");

    let info = command_with_home(exe, &home)
        .args(["extension", "info", "example-extension"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension info installed");
    assert!(info.status.success());
    assert!(info.stderr.is_empty());
    let info_stdout = String::from_utf8(info.stdout).expect("info stdout utf8");
    let info_json: serde_json::Value = serde_json::from_str(&info_stdout).expect("info json");
    assert_eq!(info_json["source_kind"], "installed");
    assert_eq!(info_json["source_path"], serde_json::Value::Null);
    assert_eq!(info_json["status"], "installed-inert");
    assert_eq!(info_json["execution_granted"], serde_json::json!(false));
    assert_eq!(info_json["requires_review"], serde_json::json!(false));
    assert_eq!(
        info_json["requires_execution_grant"],
        serde_json::json!(true)
    );
    assert!(!info_stdout.contains(extension_dir.path().to_string_lossy().as_ref()));
    assert!(!info_stdout.contains(installed_root.to_string_lossy().as_ref()));

    let search = command_with_home(exe, &home)
        .args(["extension", "search", "example"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension search installed");
    assert!(search.status.success());
    let search_stdout = String::from_utf8(search.stdout).expect("search stdout utf8");
    let search_json: serde_json::Value = serde_json::from_str(&search_stdout).expect("search json");
    let results = search_json["results"].as_array().expect("search results");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["id"], "example-extension");
    assert_eq!(results[0]["source_kind"], "installed");
    assert_eq!(results[0]["status"], "installed-inert");
    assert_eq!(results[0]["execution_granted"], serde_json::json!(false));
    assert_eq!(results[0]["requires_review"], serde_json::json!(false));
    assert_eq!(
        results[0]["requires_execution_grant"],
        serde_json::json!(true)
    );
    assert!(!search_stdout.contains(extension_dir.path().to_string_lossy().as_ref()));
    assert!(!search_stdout.contains(installed_root.to_string_lossy().as_ref()));

    let listed_after_source_removal = command_with_home(exe, &home)
        .args(["extension", "list"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension list after source removal");
    assert!(listed_after_source_removal.status.success());
    assert!(String::from_utf8(listed_after_source_removal.stdout)
        .expect("list after source removal stdout")
        .contains("example-extension installed-inert installed"));

    for args in [
        vec!["extension", "enable", "example-extension"],
        vec!["extension", "run", "example-extension.inspect", "session"],
    ] {
        let output = command_with_home(exe, &home)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("installed extension fail closed");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains(
            "installed extension is inert; reviewed execution grants are not implemented"
        ));
    }

    let disabled = command_with_home(exe, &home)
        .args(["extension", "disable", "example-extension"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("disable installed");
    assert!(!disabled.status.success());
    assert!(String::from_utf8_lossy(&disabled.stderr)
        .contains("installed extension is inert and not enabled; use uninstall"));

    let reloaded = command_with_home(exe, &home)
        .args(["extension", "reload", "example-extension"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("reload installed");
    assert!(!reloaded.status.success());
    assert!(String::from_utf8_lossy(&reloaded.stderr)
        .contains("extension id `example-extension` is installed, not linked"));

    let uninstalled = command_with_home(exe, &home)
        .args(["extension", "uninstall", "example-extension"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension uninstall");
    assert!(uninstalled.status.success());
    let uninstalled_json: serde_json::Value =
        serde_json::from_slice(&uninstalled.stdout).expect("uninstall json");
    assert_eq!(uninstalled_json["id"], "example-extension");
    assert_eq!(uninstalled_json["source_kind"], "installed");
    assert_eq!(uninstalled_json["status"], "uninstalled");
    assert!(!installed_root.exists());
    assert!(extension_dir.path().exists());

    let unknown_uninstall = command_with_home(exe, &home)
        .args(["extension", "uninstall", "example-extension"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("unknown uninstall");
    assert!(!unknown_uninstall.status.success());
    assert!(String::from_utf8_lossy(&unknown_uninstall.stderr)
        .contains("unknown installed extension id: example-extension"));
}

#[cfg(unix)]
#[test]
fn extension_install_rejects_symlinked_registry_root_without_target_write() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let extension_dir = tempfile::tempdir().expect("extension dir");
    write_extension_manifest(extension_dir.path(), "example-extension", "0.1.0");
    let target = tempfile::tempdir().expect("registry target");
    let euler_home = home.path().join(".euler");
    fs::create_dir_all(&euler_home).expect("euler home");
    symlink(target.path(), euler_home.join("extensions")).expect("registry root symlink");

    let output = command_with_home(exe, &home)
        .args(["extension", "install", path_str(extension_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension install");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("install stderr utf8");
    assert!(stderr.contains("extension registry path must not be a symlink"));
    assert_no_path_leak(&stderr, &[target.path(), extension_dir.path()]);
    assert!(target
        .path()
        .read_dir()
        .expect("target entries")
        .next()
        .is_none());
}

#[cfg(unix)]
#[test]
fn extension_link_rejects_symlinked_registry_root_without_target_write() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let extension_dir = tempfile::tempdir().expect("extension dir");
    write_extension_manifest(extension_dir.path(), "example-extension", "0.1.0");
    let target = tempfile::tempdir().expect("registry target");
    let euler_home = home.path().join(".euler");
    fs::create_dir_all(&euler_home).expect("euler home");
    symlink(target.path(), euler_home.join("extensions")).expect("registry root symlink");

    let output = command_with_home(exe, &home)
        .args(["extension", "link", path_str(extension_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension link");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("link stderr utf8");
    assert!(stderr.contains("extension registry path must not be a symlink"));
    assert_no_path_leak(&stderr, &[target.path(), extension_dir.path()]);
    assert!(target
        .path()
        .read_dir()
        .expect("target entries")
        .next()
        .is_none());
}

#[cfg(unix)]
#[test]
fn extension_link_rejects_symlinked_link_inventory_tmp_without_target_write() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let extension_dir = tempfile::tempdir().expect("extension dir");
    write_extension_manifest(extension_dir.path(), "example-extension", "0.1.0");
    let target = tempfile::NamedTempFile::new().expect("target file");
    fs::write(target.path(), b"inventory tmp sentinel\n").expect("target content");
    let registry_dir = home.path().join(".euler").join("extensions");
    fs::create_dir_all(&registry_dir).expect("registry dir");
    symlink(target.path(), registry_dir.join("links.json.tmp")).expect("inventory tmp symlink");

    let output = command_with_home(exe, &home)
        .args(["extension", "link", path_str(extension_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension link");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("link stderr utf8");
    assert!(stderr.contains("extension registry file path must not be a symlink"));
    assert_no_path_leak(&stderr, &[target.path(), extension_dir.path()]);
    assert_eq!(
        fs::read(target.path()).expect("target after"),
        b"inventory tmp sentinel\n"
    );
}

#[test]
fn extension_install_rejects_mode_conflicts() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let linked_dir = tempfile::tempdir().expect("linked dir");
    write_extension_manifest(linked_dir.path(), "example-extension", "0.1.0");
    let linked = command_with_home(exe, &home)
        .args(["extension", "link", path_str(linked_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("link");
    assert!(linked.status.success());

    let install_linked = command_with_home(exe, &home)
        .args(["extension", "install", path_str(linked_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("install linked id");
    assert!(!install_linked.status.success());
    assert!(String::from_utf8_lossy(&install_linked.stderr)
        .contains("already linked; remove it before adding it as installed"));

    let uninstall_linked = command_with_home(exe, &home)
        .args(["extension", "uninstall", "example-extension"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("uninstall linked");
    assert!(!uninstall_linked.status.success());
    assert!(String::from_utf8_lossy(&uninstall_linked.stderr)
        .contains("extension id `example-extension` is linked, not installed"));
    assert!(linked_dir.path().exists());
}

#[test]
fn extension_install_is_idempotent_and_blocks_link_shadowing() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let extension_dir = tempfile::tempdir().expect("extension dir");
    write_extension_manifest(extension_dir.path(), "example-extension", "0.1.0");

    let first = command_with_home(exe, &home)
        .args(["extension", "install", path_str(extension_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("first install");
    assert!(first.status.success());
    let first_json: serde_json::Value = serde_json::from_slice(&first.stdout).expect("first json");

    let second = command_with_home(exe, &home)
        .args(["extension", "install", path_str(extension_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("same install");
    assert!(second.status.success());
    let second_json: serde_json::Value =
        serde_json::from_slice(&second.stdout).expect("second json");
    assert_eq!(
        second_json["manifest_sha256"],
        first_json["manifest_sha256"]
    );

    write_extension_manifest(extension_dir.path(), "example-extension", "0.2.0");
    let drift = command_with_home(exe, &home)
        .args(["extension", "install", path_str(extension_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("digest drift install");
    assert!(!drift.status.success());
    assert!(String::from_utf8_lossy(&drift.stderr).contains("already installed with manifest"));

    let link_installed = command_with_home(exe, &home)
        .args(["extension", "link", path_str(extension_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("link installed");
    assert!(!link_installed.status.success());
    assert!(String::from_utf8_lossy(&link_installed.stderr)
        .contains("already installed; remove it before adding it as linked"));

    let unlink_installed = command_with_home(exe, &home)
        .args(["extension", "unlink", "example-extension"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("unlink installed");
    assert!(!unlink_installed.status.success());
    assert!(String::from_utf8_lossy(&unlink_installed.stderr)
        .contains("extension id `example-extension` is installed, not linked"));
}

#[test]
fn extension_cli_links_reloads_unlinks_and_blocks_local_runtime() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let extension_dir = tempfile::tempdir().expect("extension dir");
    write_extension_manifest(extension_dir.path(), "example-extension", "0.1.0");
    let sentinel = extension_dir.path().join("sentinel-created");
    fs::write(
        extension_dir.path().join("build.sh"),
        format!("#!/bin/sh\ntouch {}\n", sentinel.display()),
    )
    .expect("write script");

    let validated = command_with_home(exe, &home)
        .args(["extension", "validate", path_str(extension_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension validate");
    assert!(validated.status.success());
    assert!(validated.stderr.is_empty());
    let validated_json: serde_json::Value =
        serde_json::from_slice(&validated.stdout).expect("validate json");
    assert_eq!(validated_json["id"], "example-extension");
    assert_eq!(validated_json["status"], "valid");
    assert_eq!(validated_json["command_count"], 1);
    assert!(validated_json["source_path"]
        .as_str()
        .expect("source path")
        .starts_with('/'));
    assert!(!sentinel.exists(), "validate must not execute scripts");

    let linked = command_with_home(exe, &home)
        .args(["extension", "link", path_str(extension_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension link");
    assert!(linked.status.success());
    assert!(linked.stderr.is_empty());
    let linked_json: serde_json::Value = serde_json::from_slice(&linked.stdout).expect("link json");
    assert_eq!(linked_json["id"], "example-extension");
    assert_eq!(linked_json["status"], "needs-review");
    assert_eq!(linked_json["runtime_kind"], "native-rust");
    assert!(linked_json["updated_ts_ms"].as_u64().is_some());
    assert_eq!(
        linked_json["manifest_sha256"],
        validated_json["manifest_sha256"]
    );
    assert!(!sentinel.exists(), "link must not execute scripts");

    let listed = command_with_home(exe, &home)
        .args(["extension", "list"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("extension list with link");
    assert!(listed.status.success());
    let list_stdout = String::from_utf8(listed.stdout).expect("list stdout");
    assert!(list_stdout.contains("example-extension needs-review linked"));

    let info = command_with_home(exe, &home)
        .args(["extension", "info", "example-extension"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("linked info");
    assert!(info.status.success());
    assert!(info.stderr.is_empty());
    let info_json: serde_json::Value = serde_json::from_slice(&info.stdout).expect("info json");
    assert_eq!(info_json["source_kind"], "linked");
    assert_eq!(info_json["status"], "needs-review");
    // Compare against the canonicalized tempdir: the binary canonicalizes
    // linked paths, and on macOS `TempDir::path()` returns the `/var/…`
    // symlink form of `/private/var/…`.
    let canonical_extension_dir = extension_dir
        .path()
        .canonicalize()
        .expect("canonical extension dir");
    assert!(info_json["source_path"]
        .as_str()
        .expect("linked source path")
        .starts_with(canonical_extension_dir.to_string_lossy().as_ref()));
    assert_eq!(info_json["requires_review"], serde_json::json!(true));
    assert_eq!(
        info_json["requires_execution_grant"],
        serde_json::json!(false)
    );
    assert_eq!(info_json["updated_ts_ms"], linked_json["updated_ts_ms"]);
    assert_eq!(info_json["commands"][0]["name"], "inspect");

    let enable = command_with_home(exe, &home)
        .args(["extension", "enable", "example-extension"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("linked enable");
    assert!(!enable.status.success());
    assert!(enable.stdout.is_empty());
    assert!(String::from_utf8_lossy(&enable.stderr)
        .contains("linked extension runtime is not runnable yet: native-rust"));

    let run = command_with_home(exe, &home)
        .args(["extension", "run", "example-extension.inspect", "session"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("linked run");
    assert!(!run.status.success());
    assert!(run.stdout.is_empty());
    assert!(String::from_utf8_lossy(&run.stderr)
        .contains("linked extension runtime is not runnable yet: native-rust"));
    assert!(!sentinel.exists(), "run rejection must not execute scripts");

    write_extension_manifest(extension_dir.path(), "example-extension", "0.2.0");
    let reloaded = command_with_home(exe, &home)
        .args(["extension", "reload", "example-extension"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("linked reload");
    assert!(reloaded.status.success());
    let reloaded_json: serde_json::Value =
        serde_json::from_slice(&reloaded.stdout).expect("reload json");
    assert_eq!(reloaded_json["status"], "needs-review");
    assert!(reloaded_json["updated_ts_ms"].as_u64().is_some());
    assert_ne!(
        reloaded_json["manifest_sha256"],
        linked_json["manifest_sha256"]
    );

    fs::remove_file(
        extension_dir
            .path()
            .join(euler_core::EXTENSION_MANIFEST_FILE),
    )
    .expect("remove manifest");
    let broken = command_with_home(exe, &home)
        .args(["extension", "reload", "example-extension"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("broken reload");
    assert!(broken.status.success());
    let broken_json: serde_json::Value =
        serde_json::from_slice(&broken.stdout).expect("broken json");
    assert_eq!(broken_json["status"], "broken");
    assert!(broken_json["broken_reason"]
        .as_str()
        .expect("broken reason")
        .contains("io failed"));

    let unlinked = command_with_home(exe, &home)
        .args([
            "extension",
            "unlink",
            "example-extension",
            "--scope",
            "user",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("unlink");
    assert!(unlinked.status.success());
    let unlinked_json: serde_json::Value =
        serde_json::from_slice(&unlinked.stdout).expect("unlink json");
    assert_eq!(unlinked_json["status"], "unlinked");
    assert!(
        extension_dir.path().exists(),
        "unlink must not delete source"
    );
}

#[cfg(unix)]
#[test]
fn extension_cli_runs_enabled_linked_python_process_and_reload_revokes_it() {
    let python = Command::new("python3")
        .arg("--version")
        .output()
        .expect("Python 3 is required for the managed-process CLI test");
    assert!(python.status.success(), "python3 --version failed");

    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let extension_dir = tempfile::tempdir().expect("extension dir");
    let python = provision_python_venv(extension_dir.path());
    write_managed_process_extension_manifest(
        extension_dir.path(),
        "python-cli-proof",
        "0.1.1",
        &[
            python.to_string_lossy().into_owned(),
            "-B".to_owned(),
            "-u".to_owned(),
            "extension.py".to_owned(),
        ],
    );
    let script = r#"import sys
from pathlib import Path

from euler_managed_process_sdk import serve

def inspect(context):
    Path("invoked").write_text("yes", encoding="utf-8")
    sys.stderr.write("PYTHON_STDERR_SENTINEL\n")
    sys.stderr.flush()
    page = context.host.query_provenance(limit=8, scan_limit=32)
    artifact = context.host.write_artifact(
        display_name="python-cli-proof.txt",
        media_type="text/plain",
        data=b"cli artifact",
        source_event_ids=[event["id"] for event in page["events"]],
        metadata={"producer": "python-cli-proof"},
    )
    return {"input": context.input, "artifact": artifact, "seen_events": len(page["events"])}

serve({"inspect": inspect})
"#;
    fs::write(extension_dir.path().join("extension.py"), script).expect("write Python extension");

    let session_dir = tempfile::tempdir().expect("session dir");
    let log = session_dir.path().join("events.jsonl");
    write_events(&log, &[session_start("fixture", "echo")]);
    let input = session_dir.path().join("input.json");
    fs::write(&input, r#"{"tag":"cli-proof"}"#).expect("write extension input");
    let expected_entrypoint =
        serde_json::json!([python.to_string_lossy(), "-B", "-u", "extension.py"]);

    let validated = command_with_home(exe, &home)
        .args(["extension", "validate", path_str(extension_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("validate Python extension");
    assert!(validated.status.success());
    assert!(validated.stderr.is_empty());
    let validated_json: serde_json::Value =
        serde_json::from_slice(&validated.stdout).expect("validate json");
    assert_eq!(validated_json["entrypoint"]["command"], expected_entrypoint);

    let linked = command_with_home(exe, &home)
        .args(["extension", "link", path_str(extension_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("link Python extension");
    assert!(linked.status.success());
    let linked_json: serde_json::Value = serde_json::from_slice(&linked.stdout).expect("link json");
    assert_eq!(
        linked_json["runtime_kind"],
        serde_json::json!("managed-process")
    );
    assert_eq!(linked_json["status"], serde_json::json!("needs-review"));
    assert_eq!(linked_json["entrypoint"]["command"], expected_entrypoint);

    let review_info = command_with_home(exe, &home)
        .args(["extension", "info", "python-cli-proof"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("linked Python extension info");
    assert!(review_info.status.success());
    assert!(review_info.stderr.is_empty());
    let review_info_json: serde_json::Value =
        serde_json::from_slice(&review_info.stdout).expect("review info json");
    assert_eq!(
        review_info_json["status"],
        serde_json::json!("needs-review")
    );
    assert_eq!(
        review_info_json["entrypoint"]["command"],
        expected_entrypoint
    );

    let before_enable = command_with_home(exe, &home)
        .args([
            "extension",
            "run",
            "python-cli-proof.inspect",
            path_str(&log),
            "--input-file",
            path_str(&input),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run before enable");
    assert!(!before_enable.status.success());
    assert!(before_enable.stdout.is_empty());
    assert!(String::from_utf8_lossy(&before_enable.stderr).contains(
        "linked extension is not enabled; run `euler extension enable python-cli-proof` first"
    ));
    assert!(
        !extension_dir.path().join("invoked").exists(),
        "a linked package must not launch before explicit enable"
    );

    let enabled = command_with_home(exe, &home)
        .args(["extension", "enable", "python-cli-proof"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("enable Python extension");
    assert!(enabled.status.success());
    assert_eq!(
        String::from_utf8_lossy(&enabled.stdout),
        format!(
            "python-cli-proof enabled: [\"{}\",\"-B\",\"-u\",\"extension.py\"]\n",
            python.to_string_lossy()
        )
    );
    assert!(enabled.stderr.is_empty());

    let info = command_with_home(exe, &home)
        .args(["extension", "info", "python-cli-proof"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("enabled Python extension info");
    assert!(info.status.success());
    let info_json: serde_json::Value = serde_json::from_slice(&info.stdout).expect("info json");
    assert_eq!(info_json["status"], serde_json::json!("enabled"));
    assert_eq!(info_json["execution_granted"], serde_json::json!(true));
    assert_eq!(info_json["requires_review"], serde_json::json!(false));
    assert_eq!(info_json["entrypoint"]["command"], expected_entrypoint);

    let run = command_with_home(exe, &home)
        .args([
            "extension",
            "run",
            "python-cli-proof.inspect",
            path_str(&log),
            "--input-file",
            path_str(&input),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run Python extension");
    assert!(
        run.status.success(),
        "managed-process stderr: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let run_stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        run_stderr.contains(
            "extension python-cli-proof.inspect: granting declared capabilities for this run: provenance-read, artifact-write"
        ),
        "the explicit headless grant must be visible: {run_stderr}"
    );
    assert!(
        !run_stderr.contains("PYTHON_STDERR_SENTINEL"),
        "child stderr must not escape the protocol"
    );
    let run_json: serde_json::Value = serde_json::from_slice(&run.stdout).expect("run json");
    assert_eq!(run_json["input"], serde_json::json!({"tag": "cli-proof"}));
    assert!(run_json["seen_events"].as_u64().expect("seen events") >= 1);
    assert!(extension_dir.path().join("invoked").is_file());

    let events = read_jsonl(&log);
    let artifact_event = events
        .iter()
        .rev()
        .find(|event| event.kind.as_str() == EventKind::EXTENSION_ARTIFACT)
        .expect("managed-process artifact event");
    assert_eq!(
        artifact_event.payload.get("extension_id"),
        Some(&serde_json::json!("python-cli-proof"))
    );
    assert_eq!(
        artifact_event.payload.get("display_name"),
        Some(&serde_json::json!("python-cli-proof.txt"))
    );
    let relative_path = artifact_event
        .payload
        .get("path")
        .and_then(serde_json::Value::as_str)
        .expect("artifact path");
    assert_eq!(
        fs::read(session_dir.path().join(relative_path)).expect("managed-process artifact bytes"),
        b"cli artifact"
    );
    let raw_log = fs::read(&log).expect("read session log");
    assert!(
        !contains_bytes(&raw_log, b"PYTHON_STDERR_SENTINEL"),
        "raw child stderr must not enter provenance"
    );
    assert!(
        !contains_bytes(&raw_log, b"cli artifact"),
        "artifact bytes must not enter provenance"
    );

    // The stored manifest fingerprint, not a best-effort source read, is the
    // review boundary. A stale source must stop presenting as enabled and must
    // not launch until `reload` records a new review decision.
    write_managed_process_extension_manifest(
        extension_dir.path(),
        "python-cli-proof",
        "0.1.2",
        &[
            python.to_string_lossy().into_owned(),
            "-B".to_owned(),
            "-u".to_owned(),
            "extension.py".to_owned(),
        ],
    );
    let stale_info = command_with_home(exe, &home)
        .args(["extension", "info", "python-cli-proof"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("stale linked Python extension info");
    assert!(stale_info.status.success());
    let stale_info_json: serde_json::Value =
        serde_json::from_slice(&stale_info.stdout).expect("stale info json");
    assert_eq!(stale_info_json["status"], serde_json::json!("needs-review"));
    assert_eq!(
        stale_info_json["execution_granted"],
        serde_json::json!(false)
    );
    assert_eq!(stale_info_json["requires_review"], serde_json::json!(true));

    fs::remove_file(extension_dir.path().join("invoked")).expect("clear invocation marker");
    let stale_run = command_with_home(exe, &home)
        .args([
            "extension",
            "run",
            "python-cli-proof.inspect",
            path_str(&log),
            "--input-file",
            path_str(&input),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run stale Python extension");
    assert!(!stale_run.status.success());
    assert!(String::from_utf8_lossy(&stale_run.stderr).contains(
        "linked extension manifest changed; run `euler extension reload python-cli-proof`"
    ));
    assert!(
        !extension_dir.path().join("invoked").exists(),
        "a stale manifest must not launch"
    );

    let reloaded = command_with_home(exe, &home)
        .args(["extension", "reload", "python-cli-proof"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("reload Python extension");
    assert!(reloaded.status.success());
    let reloaded_json: serde_json::Value =
        serde_json::from_slice(&reloaded.stdout).expect("reload json");
    assert_eq!(reloaded_json["status"], serde_json::json!("needs-review"));

    let log_before_rejected_run = fs::read(&log).expect("log before rejected run");
    let after_reload = command_with_home(exe, &home)
        .args([
            "extension",
            "run",
            "python-cli-proof.inspect",
            path_str(&log),
            "--input-file",
            path_str(&input),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run after reload");
    assert!(!after_reload.status.success());
    assert!(String::from_utf8_lossy(&after_reload.stderr).contains(
        "linked extension is not enabled; run `euler extension enable python-cli-proof` first"
    ));
    assert!(
        !extension_dir.path().join("invoked").exists(),
        "reload must revoke the prior launch decision"
    );
    assert_eq!(
        fs::read(&log).expect("log after rejected run"),
        log_before_rejected_run
    );
}

#[test]
fn extension_cli_validate_redacts_secret_values() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let secret_dir = tempfile::tempdir().expect("secret dir");
    fs::write(
        secret_dir.path().join(euler_core::EXTENSION_MANIFEST_FILE),
        r#"{
  "version": 1,
  "id": "secret-extension",
  "display_name": "Secret Extension",
  "extension_version": "0.1.0",
  "runtime_kind": "native-rust",
  "capabilities": ["provenance-read"],
  "commands": [
    {
      "name": "inspect",
      "display_name": "Inspect",
      "summary": "Inspect provenance.",
      "required_capabilities": ["provenance-read"],
      "api_key": "SHOULD_NOT_APPEAR"
    }
  ]
}"#,
    )
    .expect("write secret manifest");

    let output = command_with_home(exe, &home)
        .args(["extension", "validate", path_str(secret_dir.path())])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("secret validate");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("forbidden secret-like field"));
    assert!(stderr.contains("api_key"));
    assert!(!stderr.contains("SHOULD_NOT_APPEAR"));
}

#[test]
fn explicit_path_resume_does_not_append_home_session_index_update() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let mut first = command_with_home(exe, &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn first euler");
    first
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"alpha explicit path\n")
        .expect("write first stdin");
    assert!(first
        .wait_with_output()
        .expect("wait first")
        .status
        .success());

    let session_id = only_home_session_id(home.path());
    let log = home_session_log(home.path(), &session_id);
    let before = home_index_line_count(home.path());
    let resumed = command_with_home(exe, &home)
        .arg("--resume")
        .arg(&log)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(b"beta explicit path\n")?;
            child.wait_with_output()
        })
        .expect("resume explicit path");

    assert!(resumed.status.success());
    assert_eq!(home_index_line_count(home.path()), before);
}

#[test]
fn explicit_provenance_stays_unindexed_and_exec_creates_non_interactive_home_session() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let work = tempfile::tempdir().expect("work dir");

    let mut home_run = command_with_home(exe, &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn home run");
    home_run
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"alpha indexed\n")
        .expect("write home stdin");
    assert!(home_run
        .wait_with_output()
        .expect("wait home")
        .status
        .success());
    let before = home_index_line_count(home.path());

    let explicit_log = work.path().join("explicit.jsonl");
    let explicit = command_with_home(exe, &home)
        .arg("--provenance")
        .arg(&explicit_log)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(b"explicit unindexed\n")?;
            child.wait_with_output()
        })
        .expect("explicit provenance");
    assert!(explicit.status.success());
    assert_eq!(home_index_line_count(home.path()), before);

    let exec = command_with_home(exe, &home)
        .current_dir(work.path())
        .arg("exec")
        .arg("exec indexed")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("exec");
    assert!(exec.status.success());
    assert_eq!(home_index_line_count(home.path()), before + 2);

    let session_ids = home_session_ids(home.path());
    assert_eq!(session_ids.len(), 2);
    let exec_id = session_ids.last().expect("exec session id");
    let exec_events = read_jsonl(&home_session_log(home.path(), exec_id));
    let start = exec_events
        .iter()
        .find(|event| event.kind.as_str() == EventKind::SESSION_START)
        .expect("exec session.start");
    assert_eq!(
        start
            .payload
            .get("session_kind")
            .and_then(serde_json::Value::as_str),
        Some("non-interactive")
    );
}

#[test]
fn resume_by_home_session_id_wins_over_colliding_name() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    for input in ["first id\n", "second named as first id\n"] {
        let mut run = command_with_home(exe, &home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn euler");
        run.stdin
            .as_mut()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write stdin");
        assert!(run.wait_with_output().expect("wait run").status.success());
    }

    let session_ids = home_session_ids(home.path());
    assert_eq!(session_ids.len(), 2);
    let id_log = home_session_log(home.path(), &session_ids[0]);
    let name_collision_log = home_session_log(home.path(), &session_ids[1]);
    append_session_rename_event(&name_collision_log, &session_ids[1], &session_ids[0]);
    let name_collision_before = fs::read(&name_collision_log).expect("read name collision before");

    let resumed = command_with_home(exe, &home)
        .arg("--resume")
        .arg(&session_ids[0])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(b"after id collision\n")?;
            child.wait_with_output()
        })
        .expect("resume by id");

    assert!(resumed.status.success());
    let id_events = read_jsonl(&id_log);
    assert!(id_events
        .iter()
        .all(|event| event.session == session_ids[0]));
    assert!(replay_transcript_with_home(exe, home.path(), &id_log)
        .contains("user: after id collision\n"));
    assert_eq!(
        fs::read(&name_collision_log).expect("read name collision after"),
        name_collision_before
    );
}

#[test]
fn resume_by_id_shaped_home_session_name_falls_back_to_name() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let id_shaped_name = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    let mut first = command_with_home(exe, &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn first euler");
    first
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"alpha id shaped\n")
        .expect("write first stdin");
    assert!(first
        .wait_with_output()
        .expect("wait first")
        .status
        .success());

    let session_id = only_home_session_id(home.path());
    assert_ne!(session_id, id_shaped_name);
    let log = home_session_log(home.path(), &session_id);
    append_session_rename_event(&log, &session_id, id_shaped_name);

    let resumed = command_with_home(exe, &home)
        .arg("--resume")
        .arg(id_shaped_name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(b"beta id shaped\n")?;
            child.wait_with_output()
        })
        .expect("resume by id shaped name");

    assert!(resumed.status.success());
    let events = read_jsonl(&log);
    assert!(events.iter().all(|event| event.session == session_id));
    let replayed = replay_transcript_with_home(exe, home.path(), &log);
    assert!(replayed.contains("user: alpha id shaped\n"));
    assert!(replayed.contains("user: beta id shaped\n"));
}

#[test]
fn resume_by_missing_home_session_reference_fails_clearly() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let output = command_with_home(exe, &home)
        .arg("--resume")
        .arg("missing session")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("resume missing");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no session found with id or name missing session"));
    assert!(!stderr.contains("\"missing session\""));
}

#[test]
fn resume_by_home_session_id_with_unknown_kind_fails_without_appending() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let mut first = command_with_home(exe, &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    first
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"before invalid\n")
        .expect("write stdin");
    assert!(first
        .wait_with_output()
        .expect("wait first")
        .status
        .success());

    let session_id = only_home_session_id(home.path());
    let log = home_session_log(home.path(), &session_id);
    append_unknown_event(&log, &session_id);

    let output = resume_home_session_expect_failure_without_log_change(&home, &session_id);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("resume incompatible: unknown event kind future.kind"));
}

#[test]
fn resume_by_home_session_id_with_malformed_line_fails_without_appending() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let mut first = command_with_home(exe, &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    first
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"before malformed\n")
        .expect("write stdin");
    assert!(first
        .wait_with_output()
        .expect("wait first")
        .status
        .success());

    let session_id = only_home_session_id(home.path());
    let log = home_session_log(home.path(), &session_id);
    append_raw_to_log(&log, b"not-json\n");

    let output = resume_home_session_expect_failure_without_log_change(&home, &session_id);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid provenance line"));
}

#[test]
fn resume_by_home_session_id_with_missing_blob_fails_without_appending() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let mut first = command_with_home(exe, &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    first
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"before missing blob\n")
        .expect("write stdin");
    assert!(first
        .wait_with_output()
        .expect("wait first")
        .status
        .success());

    let session_id = only_home_session_id(home.path());
    let log = home_session_log(home.path(), &session_id);
    append_missing_blob_event(&log, &session_id);

    let output = resume_home_session_expect_failure_without_log_change(&home, &session_id);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("resume incompatible: missing provenance blob"));
    assert!(stderr.contains(BLOB_HASH));
}

#[test]
fn resume_by_ambiguous_home_session_name_fails_without_appending() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    for input in ["first shared\n", "second shared\n"] {
        let mut run = command_with_home(exe, &home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn euler");
        run.stdin
            .as_mut()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write stdin");
        assert!(run.wait_with_output().expect("wait run").status.success());
    }

    let session_ids = home_session_ids(home.path());
    assert_eq!(session_ids.len(), 2);
    let first_log = home_session_log(home.path(), &session_ids[0]);
    let second_log = home_session_log(home.path(), &session_ids[1]);
    append_session_rename_event(&first_log, &session_ids[0], "shared session");
    append_session_rename_event(&second_log, &session_ids[1], "shared session");
    let first_before = fs::read(&first_log).expect("read first before");
    let second_before = fs::read(&second_log).expect("read second before");

    let output = command_with_home(exe, &home)
        .arg("--resume")
        .arg("shared session")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("resume ambiguous");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ambiguous session name"));
    assert!(stderr.contains("shared session"));
    assert!(stderr.contains(&session_ids[0]));
    assert!(stderr.contains(&session_ids[1]));
    assert_eq!(
        fs::read(&first_log).expect("read first after"),
        first_before
    );
    assert_eq!(
        fs::read(&second_log).expect("read second after"),
        second_before
    );
}

#[test]
fn concurrent_cli_writer_fails_with_session_locked_message() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let lock = log.with_file_name(format!(
        "{}.lock",
        log.file_name().expect("log filename").to_string_lossy()
    ));
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let mut first = command_with_home(exe, &home)
        .arg("--provider")
        .arg("fixture")
        .arg("--provenance")
        .arg(&log)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn first euler");

    wait_until_session_lock_held(&lock, &mut first);

    let second = command_with_home(exe, &home)
        .arg("--provider")
        .arg("fixture")
        .arg("--provenance")
        .arg(&log)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run second euler");

    first
        .stdin
        .as_mut()
        .expect("first stdin")
        .write_all(b"exit\n")
        .expect("stop first euler");
    let first = first.wait_with_output().expect("wait first euler");

    assert!(first.status.success());
    assert!(!second.status.success());
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("already open by another Euler process"));
    assert!(stderr.contains("Owner: PID"));
    assert!(stderr.contains("Close that process and retry."));
}

#[test]
fn killed_cli_writer_releases_advisory_lock_without_removing_lock_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let lock = lock_path_for(&log);
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let mut first = command_with_home(exe, &home)
        .arg("--provider")
        .arg("fixture")
        .arg("--provenance")
        .arg(&log)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn first euler");

    wait_until_session_lock_held(&lock, &mut first);

    first.kill().expect("kill first euler");
    first.wait().expect("reap killed euler");
    assert!(lock.exists(), "crash leaves the persistent lock file");

    let retried = command_with_home(exe, &home)
        .arg("--provider")
        .arg("fixture")
        .arg("--provenance")
        .arg(&log)
        .stdin(Stdio::null())
        .output()
        .expect("retry after killed owner");
    assert!(
        retried.status.success(),
        "OS should release the advisory lock when its owner is killed: {}",
        String::from_utf8_lossy(&retried.stderr)
    );
}

#[test]
fn replaying_provenance_reproduces_the_transcript() {
    // Acceptance criterion 3: the transcript is a projection of the event
    // stream. Run a live fixture session, then re-render purely from the
    // JSONL log and require byte equality.
    let temp = tempfile::tempdir().expect("temp dir");
    let provenance = temp.path().join("events.jsonl");

    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let mut child = command_with_home(exe, &home)
        .arg("--provenance")
        .arg(&provenance)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"hello replay\n")
        .expect("write stdin");
    let live = child.wait_with_output().expect("live run");
    assert!(live.status.success());

    let replayed = command_with_home(exe, &home)
        .arg("--replay")
        .arg(&provenance)
        .output()
        .expect("replay run");

    assert!(replayed.status.success());
    let live_text = String::from_utf8_lossy(&live.stdout);
    let replay_text = String::from_utf8_lossy(&replayed.stdout);
    assert!(!live_text.is_empty());
    assert_eq!(live_text, replay_text);
}

#[test]
fn resume_then_next_turn_matches_uninterrupted_transcript_projection() {
    let temp = tempfile::tempdir().expect("temp dir");
    let resumed_log = temp.path().join("resumed.jsonl");
    let uninterrupted_log = temp.path().join("uninterrupted.jsonl");
    let exe = env!("CARGO_BIN_EXE_euler");

    // One shared workspace for create and resume: sessions resume only from
    // the folder they were recorded in (ADR 0017).
    let first = run_euler_with_input_from(
        exe,
        &["--provenance", path_str(&resumed_log)],
        "alpha\n",
        temp.path(),
    );
    assert!(first.status.success());

    let resumed = run_euler_with_input_from(
        exe,
        &["--resume", path_str(&resumed_log)],
        "beta\n",
        temp.path(),
    );
    assert!(resumed.status.success());
    let stderr = String::from_utf8_lossy(&resumed.stderr);
    assert!(stderr.contains("resumed session headless-session"));
    assert!(stderr.contains("folded 7 events"));
    assert!(stderr.contains("target fixture/echo"));
    assert!(stderr.contains("recovery closure not appended"));

    let uninterrupted = run_euler_with_input(
        exe,
        &["--provenance", path_str(&uninterrupted_log)],
        "alpha\nbeta\n",
    );
    assert!(uninterrupted.status.success());

    let resumed_replay = replay_transcript(exe, &resumed_log);
    let uninterrupted_replay = replay_transcript(exe, &uninterrupted_log);
    assert_eq!(resumed_replay, uninterrupted_replay);
}

#[test]
fn resume_with_session_start_differing_target_warns_and_uses_original() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(&log, &[session_start("fixture", "echo")]);
    let exe = env!("CARGO_BIN_EXE_euler");

    let output = run_euler_with_input(
        exe,
        &[
            "--resume",
            path_str(&log),
            "--provider",
            "fixture",
            "--model",
            "other",
        ],
        "",
    );

    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(
        "warning: resume invocation target fixture/other differs from original session target fixture/echo; using original target"
    ));
    assert!(stderr.contains("target fixture/echo"));
}

#[test]
fn resume_appends_one_recovery_closure_and_second_resume_is_idempotent() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let call = tool_call("call-read", "read_file");
    write_events(&log, &[call]);
    let before = fs::read(&log).expect("read before");
    let exe = env!("CARGO_BIN_EXE_euler");

    let first = run_euler_with_input(exe, &["--resume", path_str(&log)], "");
    assert!(first.status.success());
    assert_ne!(fs::read(&log).expect("read after first"), before);
    assert_eq!(recovery_closure_count(&read_jsonl(&log)), 1);

    let after_first = fs::read(&log).expect("read after first");
    let second = run_euler_with_input(exe, &["--resume", path_str(&log)], "");
    assert!(second.status.success());
    assert_eq!(fs::read(&log).expect("read after second"), after_first);
    assert_eq!(recovery_closure_count(&read_jsonl(&log)), 1);
}

#[test]
fn resume_unknown_kind_fails_without_appending() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(
        &log,
        &[EventEnvelope::new(
            "s",
            "a",
            None,
            "future.kind",
            object([]),
        )],
    );

    let output = resume_expect_failure_without_log_change(&log);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("resume incompatible: unknown event kind future.kind"));
}

#[test]
fn resume_missing_blob_fails_without_appending() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_blob_reference_event(&log);

    let output = resume_expect_failure_without_log_change(&log);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("resume incompatible: missing provenance blob"));
    assert!(stderr.contains(BLOB_HASH));
}

#[test]
fn resume_corrupted_blob_fails_without_appending() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_blob_reference_event(&log);
    let blobs = temp.path().join("blobs");
    fs::create_dir_all(&blobs).expect("blob dir");
    fs::write(blobs.join(BLOB_HASH), "corrupt").expect("corrupt blob");

    let output = resume_expect_failure_without_log_change(&log);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("resume incompatible: provenance blob hash mismatch"));
    assert!(stderr.contains(BLOB_HASH));
}

#[test]
fn replay_missing_blob_exits_nonzero_and_names_blob() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_blob_reference_event(&log);
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let output = command_with_home(exe, &home)
        .arg("--replay")
        .arg(&log)
        .output()
        .expect("replay run");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing provenance blob"));
    assert!(stderr.contains(BLOB_HASH));
}

#[test]
fn replay_corrupted_blob_exits_nonzero_and_names_blob() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_blob_reference_event(&log);
    let blobs = temp.path().join("blobs");
    fs::create_dir_all(&blobs).expect("blob dir");
    fs::write(blobs.join(BLOB_HASH), "corrupt").expect("corrupt blob");
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let output = command_with_home(exe, &home)
        .arg("--replay")
        .arg(&log)
        .output()
        .expect("replay run");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("provenance blob hash mismatch"));
    assert!(stderr.contains(BLOB_HASH));
}

#[test]
fn failed_live_resume_preflight_releases_lock_and_preserves_log() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let switch = EventEnvelope::new(
        "s",
        "a",
        None,
        EventKind::MODEL_SWITCHED,
        object([
            ("from_provider", "chatgpt".into()),
            ("from_model", "gpt-5.5".into()),
            ("to_provider", "fixture".into()),
            ("to_model", "echo".into()),
            ("reason", "resume-test".into()),
        ]),
    );
    write_events(&log, &[switch]);
    let before = fs::read(&log).expect("read before");
    let lock = lock_path_for(&log);
    let exe = env!("CARGO_BIN_EXE_euler");

    let failed = run_euler_with_input(
        exe,
        &["--resume", path_str(&log), "--provider", "chatgpt"],
        "",
    );

    assert!(!failed.status.success());
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("resume requires provider fixture but this invocation configures chatgpt")
    );
    assert_eq!(fs::read(&log).expect("read after failed preflight"), before);
    assert!(lock.exists(), "the advisory lock file remains persistent");

    let retried = run_euler_with_input(exe, &["--resume", path_str(&log)], "");
    assert!(retried.status.success());
}

#[test]
fn concurrent_cli_resume_fails_with_session_locked_message() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    write_events(
        &log,
        &[EventEnvelope::new(
            "s",
            "a",
            None,
            EventKind::USER_MESSAGE,
            object([("content", "held".into())]),
        )],
    );
    let lock = lock_path_for(&log);
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();

    let mut first = command_with_home(exe, &home)
        .arg("--resume")
        .arg(&log)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn first resume");

    wait_until_session_lock_held(&lock, &mut first);

    let second = command_with_home(exe, &home)
        .arg("--resume")
        .arg(&log)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run second resume");

    first
        .stdin
        .as_mut()
        .expect("first stdin")
        .write_all(b"exit\n")
        .expect("stop first resume");
    let first = first.wait_with_output().expect("wait first resume");

    assert!(first.status.success());
    assert!(!second.status.success());
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("already open by another Euler process"));
    assert!(stderr.contains("Owner: PID"));
    assert!(stderr.contains("Close that process and retry."));
}

#[test]
fn replaying_permission_events_projects_transcript_to_stdout() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provenance = temp.path().join("events.jsonl");
    let events = [
        EventEnvelope::new(
            "s",
            "a",
            None,
            EventKind::USER_MESSAGE,
            object([("content", "run command".into())]),
        ),
        EventEnvelope::new(
            "s",
            "a",
            None,
            EventKind::PERMISSION_PROMPT,
            object([
                ("capability", "shell-exec".into()),
                ("reason", "tool run_shell".into()),
            ]),
        ),
        EventEnvelope::new(
            "s",
            "a",
            None,
            EventKind::PERMISSION_DECISION,
            object([
                ("capability", "shell-exec".into()),
                ("decision", "allowed".into()),
                ("allowed", true.into()),
            ]),
        ),
    ];
    let jsonl = events
        .iter()
        .map(|event| event.to_json_line().expect("serialize event"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&provenance, format!("{jsonl}\n")).expect("write provenance");

    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let replayed = command_with_home(exe, &home)
        .arg("--replay")
        .arg(&provenance)
        .output()
        .expect("replay run");

    assert!(replayed.status.success());
    assert!(replayed.stderr.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&replayed.stdout),
        "user: run command\npermission.prompt: shell-exec\npermission.decision: allowed\n"
    );
}

#[test]
fn replay_warns_and_skips_unknown_event_kinds() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provenance = temp.path().join("events.jsonl");
    let events = [
        EventEnvelope::new(
            "s",
            "a",
            None,
            EventKind::USER_MESSAGE,
            object([("content", "hello future".into())]),
        ),
        EventEnvelope::new("s", "a", None, "future.kind", object([])),
    ];
    write_events(&provenance, &events);

    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let replayed = command_with_home(exe, &home)
        .arg("--replay")
        .arg(&provenance)
        .output()
        .expect("replay run");

    assert!(replayed.status.success());
    assert_eq!(
        String::from_utf8_lossy(&replayed.stdout),
        "user: hello future\n"
    );
    assert!(String::from_utf8_lossy(&replayed.stderr)
        .contains("warning: skipping unknown event kind future.kind"));
}

#[test]
fn replay_ignores_truncated_final_jsonl_line() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provenance = temp.path().join("events.jsonl");
    let user = EventEnvelope::new(
        "s",
        "a",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "hello".into())]),
    );
    let assistant = EventEnvelope::new(
        "s",
        "a",
        Some(user.id.clone()),
        EventKind::ASSISTANT_MESSAGE,
        object([("content", "hi".into())]),
    );
    let mut jsonl = [user, assistant]
        .iter()
        .map(|event| event.to_json_line().expect("serialize event"))
        .collect::<Vec<_>>()
        .join("\n");
    jsonl.push_str("\n{\"v\":1,\"id\":\"truncated\"");
    fs::write(&provenance, jsonl).expect("write provenance");

    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let replayed = command_with_home(exe, &home)
        .arg("--replay")
        .arg(&provenance)
        .output()
        .expect("replay run");

    assert!(replayed.status.success());
    assert!(replayed.stderr.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&replayed.stdout),
        "user: hello\nassistant: hi\n"
    );
}

#[test]
fn replay_ignores_malformed_final_jsonl_line_without_trailing_newline() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provenance = temp.path().join("events.jsonl");
    let user = EventEnvelope::new(
        "s",
        "a",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "hello".into())]),
    );
    let jsonl = format!(
        "{}\n{{\"v\":1,\"id\":\"malformed\"}}",
        user.to_json_line().expect("serialize event")
    );
    fs::write(&provenance, jsonl).expect("write provenance");

    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let replayed = command_with_home(exe, &home)
        .arg("--replay")
        .arg(&provenance)
        .output()
        .expect("replay run");

    assert!(replayed.status.success());
    assert!(replayed.stderr.is_empty());
    assert_eq!(String::from_utf8_lossy(&replayed.stdout), "user: hello\n");
}

#[test]
fn replay_rejects_malformed_final_jsonl_line_with_trailing_newline() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provenance = temp.path().join("events.jsonl");
    let user = EventEnvelope::new(
        "s",
        "a",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "hello".into())]),
    );
    let jsonl = format!(
        "{}\n{{\"v\":1,\"id\":\"malformed\"}}\n",
        user.to_json_line().expect("serialize event")
    );
    fs::write(&provenance, jsonl).expect("write provenance");

    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let replayed = command_with_home(exe, &home)
        .arg("--replay")
        .arg(&provenance)
        .output()
        .expect("replay run");

    assert!(!replayed.status.success());
    assert!(String::from_utf8_lossy(&replayed.stderr).contains("invalid provenance line"));
}

#[test]
fn replay_rejects_invalid_non_final_line_followed_by_trailing_whitespace() {
    let temp = tempfile::tempdir().expect("temp dir");
    let provenance = temp.path().join("events.jsonl");
    let user = EventEnvelope::new(
        "s",
        "a",
        None,
        EventKind::USER_MESSAGE,
        object([("content", "hello".into())]),
    );
    let jsonl = format!(
        "{}\n{{\"v\":1,\"id\":\"truncated\"\n   ",
        user.to_json_line().expect("serialize event")
    );
    fs::write(&provenance, jsonl).expect("write provenance");

    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let replayed = command_with_home(exe, &home)
        .arg("--replay")
        .arg(&provenance)
        .output()
        .expect("replay run");

    assert!(!replayed.status.success());
    assert!(String::from_utf8_lossy(&replayed.stderr).contains("invalid provenance line"));
}

/// Reconstruct the terminal's final state (scrollback + visible screen) from
/// raw PTY bytes. Legit repaints overwrite in place; only real emissions
/// survive here — so a line appearing twice means it was committed twice.
fn pty_final_state_text(output: &[u8], rows: u16, cols: u16) -> String {
    let mut parser = vt100::Parser::new(rows, cols, 5000);
    parser.process(output);
    // Clamp to the actual scrollback length.
    parser.set_scrollback(usize::MAX);
    let total_scrollback = parser.screen().scrollback();
    let mut lines: Vec<String> = Vec::new();
    let mut offset = total_scrollback;
    loop {
        parser.set_scrollback(offset);
        let contents = parser.screen().contents();
        let screen_rows: Vec<&str> = contents.lines().collect();
        if offset == total_scrollback {
            lines.extend(screen_rows.iter().map(|row| row.to_string()));
        } else {
            // Overlapping windows: keep only the rows that scrolled into view.
            let new_rows = usize::from(rows).min(total_scrollback - offset + usize::from(rows));
            let skip = screen_rows.len().saturating_sub(new_rows);
            let _ = skip;
            // Simpler: rebuild from scratch below.
            lines.clear();
            break;
        }
        if offset == 0 {
            break;
        }
        offset = offset.saturating_sub(usize::from(rows));
        if total_scrollback - offset < usize::from(rows) {
            offset = 0;
        }
    }
    if !lines.is_empty() {
        let mut all_rows = extract_bridge_committed_rows(output);
        all_rows.push(lines.join("\n"));
        return all_rows.join("\n");
    }
    // Fallback path: walk row windows without overlap bookkeeping errors by
    // stepping exactly one row at a time and keeping the top row of each view.
    let mut rebuilt: Vec<String> = Vec::new();
    let mut offset = total_scrollback;
    loop {
        parser.set_scrollback(offset);
        let contents = parser.screen().contents();
        let mut screen_rows = contents.lines();
        if let Some(top) = screen_rows.next() {
            rebuilt.push(top.to_string());
        }
        if offset == 0 {
            // The remaining visible rows complete the picture.
            rebuilt.extend(contents.lines().skip(1).map(|row| row.to_string()));
            break;
        }
        offset -= 1;
    }
    let mut all_rows = extract_bridge_committed_rows(output);
    all_rows.push(rebuilt.join("\n"));
    all_rows.join("\n")
}

/// Committed rows written through the codex-style bridge contract
/// (`ESC[1;Nr` … `ESC[r`, one row per `\r\n`). Real terminals push these into
/// native scrollback when the region top is row 1; the vt100 crate discards
/// them, so reconstruction captures them straight from the byte stream.
fn extract_bridge_committed_rows(output: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(output);
    let mut rows = Vec::new();
    let mut rest = text.as_ref();
    while let Some(start) = rest.find("\u{1b}[1;") {
        let after = &rest[start..];
        let Some(region_close) = after.find('r') else {
            break;
        };
        // Confirm this is a scroll-region set: ESC[1;<digits>r
        // (the needle is ESC [ 1 ; — digits start at byte 4)
        if region_close <= 4
            || !after[4..region_close]
                .bytes()
                .all(|byte| byte.is_ascii_digit())
        {
            rest = &rest[start + 4..];
            continue;
        }
        let Some(end) = after.find("\u{1b}[r") else {
            break;
        };
        let span = &after[region_close + 1..end];
        // The bridge writes `ESC[row;1H` then rows separated by \r\n with
        // only SGR styling in between. Ordinary viewport repaints also run
        // inside scroll-region scopes but are full of cursor movement —
        // reject any span chunk that still contains non-SGR control
        // sequences after SGR stripping (false positives counted one
        // "committed" copy per resize repaint).
        let mut chunks = span.split("\r\n");
        let header = chunks.next().unwrap_or_default();
        let header_is_bridge_move = {
            let stripped = strip_sgr(header);
            let mut ok = stripped.starts_with('\u{1b}');
            if ok {
                let body = stripped
                    .trim_start_matches('\u{1b}')
                    .trim_start_matches('[');
                ok = body
                    .trim_end_matches('H')
                    .chars()
                    .all(|ch| ch.is_ascii_digit() || ch == ';')
                    && body.ends_with('H');
            }
            ok || stripped.trim().is_empty()
        };
        if header_is_bridge_move {
            for line in chunks {
                let sgr_stripped = strip_erase_line(&strip_sgr(line));
                if sgr_stripped.contains('\u{1b}') {
                    // Cursor movement inside the span: not a bridge write.
                    continue;
                }
                let plain = strip_ansi(line);
                if !plain.trim().is_empty() {
                    rows.push(plain);
                }
            }
        }
        rest = &after[end + 3..];
    }
    rows
}

/// Remove only SGR (`ESC[...m`) sequences, keeping other controls visible.
fn strip_sgr(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(idx) = rest.find('\u{1b}') {
        out.push_str(&rest[..idx]);
        let tail = &rest[idx..];
        if let Some(after_bracket) = tail.strip_prefix("\u{1b}[") {
            if let Some(end) = after_bracket.find(|ch: char| ch.is_ascii_alphabetic()) {
                let terminator = after_bracket.as_bytes()[end] as char;
                if terminator == 'm' {
                    rest = &after_bracket[end + 1..];
                    continue;
                }
            }
        }
        // Not SGR: keep the ESC visible for the caller's rejection check.
        out.push('\u{1b}');
        rest = &tail['\u{1b}'.len_utf8()..];
    }
    out.push_str(rest);
    out
}

/// Remove erase-to-eol (`ESC[K`, `ESC[0K`) which bridge row writes use.
fn strip_erase_line(text: &str) -> String {
    text.replace("\u{1b}[K", "").replace("\u{1b}[0K", "")
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for control in chars.by_ref() {
                    if control.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') {
                chars.next();
                for control in chars.by_ref() {
                    if control == '\u{7}' {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

/// Post-purge byte segment, the (rows, cols) size at the cut, and the
/// resizes inside the segment rebased to it.
struct PostPurgeSegment<'a> {
    segment: &'a [u8],
    size: (u16, u16),
    resizes: Vec<(usize, u16, u16)>,
}

/// A debounced post-resize replay purges euler-emitted scrollback (ESC[3J)
/// and re-emits the transcript once at the settled width (issue #38). The
/// FINAL state is therefore everything from the last purge onward; bridge
/// rows and vt100 scrollback before it were erased on the real terminal.
fn pty_post_purge_segment<'a>(
    output: &'a [u8],
    initial: (u16, u16),
    resizes: &[(usize, u16, u16)],
) -> PostPurgeSegment<'a> {
    let cut = output
        .windows(4)
        .rposition(|window| window == b"\x1b[3J")
        .map(|at| at + 4)
        .unwrap_or(0);
    let size_at_cut = resizes
        .iter()
        .take_while(|(offset, _, _)| *offset <= cut)
        .last()
        .map_or(initial, |(_, rows, cols)| (*rows, *cols));
    let remaining: Vec<(usize, u16, u16)> = resizes
        .iter()
        .filter(|(offset, _, _)| *offset > cut)
        .map(|(offset, rows, cols)| (offset - cut, *rows, *cols))
        .collect();
    PostPurgeSegment {
        segment: &output[cut..],
        size: size_at_cut,
        resizes: remaining,
    }
}

/// Final-state reconstruction across mid-session resizes: process each byte
/// segment at its dimensions.
fn pty_final_state_with_resizes(
    output: &[u8],
    initial: (u16, u16),
    resizes: &[(usize, u16, u16)],
) -> String {
    let cut = pty_post_purge_segment(output, initial, resizes);
    let mut all_rows = extract_bridge_committed_rows(cut.segment);
    all_rows.push(pty_rebuild_emulator_state(
        cut.segment,
        cut.size,
        &cut.resizes,
    ));
    all_rows.join("\n")
}

/// Emulator-only final state (scrollback + screen) after the last purge —
/// no raw bridge-byte extraction. Bridge extraction counts every committed
/// row the app WROTE, even rows whose physical placement a real terminal
/// then lost (e.g. a scroll-region insert that scrolled blank rows into
/// scrollback while the viewport draw overpainted the inserted rows).
/// Assertions about what a user can actually still SEE after the settled
/// replay must use this.
fn pty_emulator_final_state(
    output: &[u8],
    initial: (u16, u16),
    resizes: &[(usize, u16, u16)],
) -> String {
    let cut = pty_post_purge_segment(output, initial, resizes);
    pty_rebuild_emulator_state(cut.segment, cut.size, &cut.resizes)
}

fn pty_rebuild_emulator_state(
    output: &[u8],
    initial: (u16, u16),
    resizes: &[(usize, u16, u16)],
) -> String {
    let (mut rows, mut cols) = initial;
    let mut parser = vt100::Parser::new(rows, cols, 5000);
    let mut start = 0usize;
    for (offset, new_rows, new_cols) in resizes {
        parser.process(&output[start..*offset]);
        parser.set_size(*new_rows, *new_cols);
        start = *offset;
        rows = *new_rows;
        cols = *new_cols;
    }
    parser.process(&output[start..]);
    let _ = (rows, cols);
    parser.set_scrollback(usize::MAX);
    let total_scrollback = parser.screen().scrollback();
    let mut rebuilt: Vec<String> = Vec::new();
    let mut offset = total_scrollback;
    loop {
        parser.set_scrollback(offset);
        let contents = parser.screen().contents();
        let mut screen_rows = contents.lines();
        if let Some(top) = screen_rows.next() {
            rebuilt.push(top.to_string());
        }
        if offset == 0 {
            rebuilt.extend(contents.lines().skip(1).map(|row| row.to_string()));
            break;
        }
        offset -= 1;
    }
    rebuilt.join("\n")
}

/// The visible screen rows (no scrollback) after the last scrollback purge,
/// parsed at the size in effect at that point.
fn pty_final_screen_rows(
    output: &[u8],
    initial: (u16, u16),
    resizes: &[(usize, u16, u16)],
) -> Vec<String> {
    let cut = pty_post_purge_segment(output, initial, resizes);
    // The settled purge follows the final resize by design (450ms trailing
    // debounce), so the post-purge segment is parsed at one fixed size.
    assert!(
        cut.resizes.is_empty(),
        "resize delivered after the settled purge — reconstruction size would be wrong"
    );
    let (rows, cols) = cut.size;
    let mut parser = vt100::Parser::new(rows, cols, 5000);
    parser.process(cut.segment);
    parser
        .screen()
        .contents()
        .lines()
        .map(|row| row.to_string())
        .collect()
}

#[test]
fn tui_pty_resize_does_not_duplicate_committed_lines() {
    // Regression target for the duplicate-line audit finding (P1): a terminal
    // resize while history has been committed to native scrollback must not
    // re-emit already-committed lines.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut events = Vec::new();
    for paragraph in 1..=6 {
        let sentence = format!(
            "Paragraph {paragraph}: streaming content long enough to wrap and \
             scroll so history rows land in native scrollback before resize."
        );
        for chunk in sentence.as_bytes().chunks(8) {
            events.push(serde_json::json!({
                "text_delta": String::from_utf8_lossy(chunk)
            }));
        }
        events.push(serde_json::json!({"text_delta": "\n\n"}));
    }
    events.push(serde_json::json!({"finished": {"stop_reason": "completed"}}));
    let second_response = serde_json::json!({"events": [
        {"text_delta": "Post-resize response committed once."},
        {"finished": {"stop_reason": "completed"}},
    ]});
    let script = write_fixture_script(
        temp.path(),
        "resize-stream.json",
        &serde_json::json!({
            "version": 1,
            "responses": [{"events": events}, second_response]
        })
        .to_string(),
    );
    let script_option = format!("event-script={}", path_str(&script));
    let mut tui = PtyHarness::spawn_with_args(
        temp.path(),
        &[
            "tui",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
        ],
    );
    assert!(
        tui.wait_for_screen("/ commands"),
        "initial TUI did not render:\n{}",
        tui.screen_text()
    );
    tui.write("overview please\r");
    assert!(
        tui.wait_for_screen("Paragraph 6:"),
        "streamed response did not render:\n{}",
        tui.screen_text()
    );
    assert!(
        tui.wait_for_home_session_event_count(temp.path(), EventKind::ASSISTANT_MESSAGE, 1),
        "first turn did not commit its assistant message before resize:\n{}",
        tui.screen_text()
    );
    // Resize after the turn completed and history is committed.
    let resize_output_offset = tui.resize(24, 100);
    assert!(
        tui.wait_for_output_after(resize_output_offset, b"\x1b[3J"),
        "resize did not complete its settled history replay:\n{}",
        tui.screen_text()
    );
    // Provoke a post-resize repaint + another committed event.
    tui.write("second message\r");
    assert!(
        tui.wait_for_screen("Post-resize response committed once."),
        "post-resize response did not render:\n{}",
        tui.screen_text()
    );
    tui.quit();

    let resizes = tui.resizes.clone();
    let final_state = pty_final_state_with_resizes(&tui.output, (24, 80), &resizes);
    let mut failures = Vec::new();
    for paragraph in 1..=6 {
        let needle = format!("Paragraph {paragraph}:");
        let occurrences = final_state
            .lines()
            .filter(|line| line.contains(&needle))
            .count();
        if occurrences != 1 {
            failures.push(format!("`{needle}` committed {occurrences}× (expected 1)"));
        }
    }
    let banner_caption = final_state
        .lines()
        .filter(|line| line.contains("e^(iπ) + 1 = 0"))
        .count();
    if banner_caption != 1 {
        failures.push(format!(
            "banner caption committed {banner_caption}× (expected 1)"
        ));
    }
    assert!(
        failures.is_empty(),
        "resize duplicated committed lines:\n{}\nFinal state:\n{final_state}",
        failures.join("\n")
    );
}

#[test]
fn tui_pty_mid_turn_input_steers_before_the_next_round() {
    // Issue #146: a message typed while a turn is in flight is absorbed at
    // the next round boundary as a canonical user.message — the model sees
    // it in-turn — instead of waiting for the turn to complete. The fixture
    // holds round 1 open with a sleep so the steering keystrokes land
    // deterministically mid-round.
    let temp = tempfile::tempdir().expect("temp dir");
    let script = write_fixture_script(
        temp.path(),
        "steering-transcript.json",
        &serde_json::json!({
            "version": 1,
            "responses": [
                {"events": [
                    {"text_delta": "phase one underway\n"},
                    {"sleep_ms": 4000},
                    {"tool_call": {
                        "id": "call-read",
                        "name": "read_file",
                        "input": {"path": "Cargo.toml"}
                    }},
                    {"finished": {"stop_reason": "tool_use"}}
                ]},
                {"events": [
                    {"text_delta": "final answer after steering"},
                    {"finished": {"stop_reason": "completed"}}
                ]}
            ]
        })
        .to_string(),
    );
    let script_option = format!("event-script={}", path_str(&script));
    let mut tui = PtyHarness::spawn_with_args(
        temp.path(),
        &[
            "tui",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
        ],
    );
    assert!(tui.wait_for_screen("/ commands"));
    // Steering is typed immediately behind the submit: the app processes
    // serial PTY input in order, so by the time these keystrokes are
    // handled the turn is in flight and its steering generation is armed
    // (spawn arms it on the UI thread before the worker exists). The
    // scripted 4s sleep in round 1 then dwarfs any scheduling jitter, so
    // the entry is queued long before the turn's next round boundary — no
    // wall-clock screen-wait involved. (The `⏎ steer` footer copy is
    // asserted by the status unit test; waiting on that glyph row proved
    // flaky on CI renderers and is not what this test is about.)
    tui.write("start the task\r");
    tui.write("steer toward the tests\r");
    assert!(
        tui.wait_for_screen("phase one underway"),
        "round 1 did not start:\n{}",
        tui.screen_text()
    );
    assert!(
        tui.wait_for_screen("final answer after steering"),
        "turn did not finish:\n{}",
        tui.screen_text()
    );
    tui.quit();

    // The durable stream shows the steering user.message inside the turn:
    // after round 1's tool result, before round 2's model call.
    let session_id = only_home_session_id(temp.path());
    let events = read_jsonl(&home_session_log(temp.path(), &session_id));
    let steering_index = events
        .iter()
        .position(|event| {
            event.kind.as_str() == "user.message"
                && event
                    .payload
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    == Some("steer toward the tests")
        })
        .expect("steering user.message persisted");
    let model_call_indexes: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.kind.as_str() == "model.call")
        .map(|(index, _)| index)
        .collect();
    // Absorbed at whichever round boundary came first after the keystrokes
    // (round 1's on fast machines, round 2's on slow ones) — and never as
    // a turn of its own: exactly two model calls proves the pre-steering
    // failure mode (queue flushed into a third turn after completion) did
    // not happen.
    assert_eq!(
        model_call_indexes.len(),
        2,
        "steering must not spawn its own turn"
    );
    assert!(
        steering_index < model_call_indexes[1],
        "steering was not absorbed in-turn: user.message at {steering_index}, \
         second model.call at {}",
        model_call_indexes[1]
    );
}

#[test]
fn tui_pty_tool_round_commits_canonical_narration_without_corruption() {
    let temp = tempfile::tempdir().expect("temp dir");
    let script = write_fixture_script(
        temp.path(),
        "tool-round-transcript.json",
        &serde_json::json!({
            "version": 1,
            "responses": [
                {"events": [
                    {"text_delta": "## Provider "},
                    {"text_delta": "map α"},
                    {"text_delta": "β\n\n| API | Pur"},
                    {"text_delta": "pose |\n|---|---|\n| invoke_"},
                    {"text_delta": "http_error | 😀 faithful |\n"},
                    {"tool_call": {
                        "id": "call-read",
                        "name": "read_file",
                        "input": {"path": "Cargo.toml"}
                    }},
                    {"finished": {"stop_reason": "tool_use"}}
                ]},
                {"events": [
                    {"text_delta": "Final: `eet"},
                    {"text_delta": "ionsStream` remains intact."},
                    {"finished": {"stop_reason": "completed"}}
                ]}
            ]
        })
        .to_string(),
    );
    let script_option = format!("event-script={}", path_str(&script));
    let mut tui = PtyHarness::spawn_with_args(
        temp.path(),
        &[
            "tui",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
        ],
    );
    assert!(tui.wait_for_screen("/ commands"));
    tui.write("inspect the provider\r");
    assert!(
        tui.wait_for_screen("Final: eetionsStream remains intact."),
        "tool round did not finish:\n{}",
        tui.screen_text()
    );
    tui.quit();

    let final_state = pty_final_state_with_resizes(&tui.output, (24, 80), &tui.resizes);
    for needle in [
        "Provider map αβ",
        "invoke_http_error",
        "😀 faithful",
        "Final: eetionsStream remains intact.",
    ] {
        assert_eq!(
            final_state.matches(needle).count(),
            1,
            "`{needle}` was lost, duplicated, or corrupted:\n{final_state}"
        );
    }
}

#[test]
fn tui_pty_streamed_turn_issues_no_cursor_position_reports() {
    // Performance regression target: every scrollback commit used to fire a
    // synchronous DSR (`ESC[6n`) cursor-position round-trip, blocking the UI
    // thread up to crossterm's ~2s timeout on each commit — measured as a
    // back-to-back burst on a single Enter press. In steady state the renderer
    // tracks the cursor itself, so a normal streamed turn must send ZERO
    // `ESC[6n` queries. The harness auto-answers cursor reports on the input
    // channel, so their answers never land in `output`; counting `ESC[6n` in
    // `output` therefore measures queries SENT (not answered), distinguishing
    // "no query sent" from "query answered".
    let temp = tempfile::tempdir().expect("temp dir");
    // Second response streams several wrapping paragraphs so many rows scroll
    // off the active region and commit to native scrollback — the exact path
    // that used to query per commit.
    let mut stream_events = Vec::new();
    for paragraph in 1..=6 {
        let sentence = format!(
            "Paragraph {paragraph}: streaming content long enough to wrap and \
             scroll so history rows land in native scrollback and each commit \
             exercises the cursor-restore path."
        );
        for chunk in sentence.as_bytes().chunks(8) {
            stream_events.push(serde_json::json!({
                "text_delta": String::from_utf8_lossy(chunk)
            }));
        }
        stream_events.push(serde_json::json!({"text_delta": "\n\n"}));
    }
    stream_events.push(serde_json::json!({"finished": {"stop_reason": "completed"}}));
    let script = write_fixture_script(
        temp.path(),
        "no-dsr-stream.json",
        &serde_json::json!({
            "version": 1,
            "responses": [
                {"events": [
                    {"text_delta": "Warmup response committed."},
                    {"finished": {"stop_reason": "completed"}},
                ]},
                {"events": stream_events},
            ]
        })
        .to_string(),
    );
    let script_option = format!("event-script={}", path_str(&script));
    let mut tui = PtyHarness::spawn_with_args(
        temp.path(),
        &[
            "tui",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
        ],
    );
    assert!(
        tui.wait_for_screen("/ commands"),
        "initial TUI did not render:\n{}",
        tui.screen_text()
    );
    // Warm up past initial attach: the very first commit (before any draw has
    // established an authoritative cursor position) is the one place tracked
    // state is genuinely unknown and a query is legitimate. Retire it here so
    // the measured window is pure steady state.
    tui.write("warmup\r");
    assert!(
        tui.wait_for_screen("Warmup response committed."),
        "warmup response did not render:\n{}",
        tui.screen_text()
    );
    assert!(
        tui.wait_for_home_session_event_count(temp.path(), EventKind::ASSISTANT_MESSAGE, 1),
        "warmup turn did not commit its assistant message:\n{}",
        tui.screen_text()
    );

    // Everything from here is the steady-state turn under measurement.
    let measure_from = tui.output.len();
    tui.write("stream several paragraphs\r");
    assert!(
        tui.wait_for_screen("Paragraph 6:"),
        "streamed response did not render:\n{}",
        tui.screen_text()
    );
    assert!(
        tui.wait_for_home_session_event_count(temp.path(), EventKind::ASSISTANT_MESSAGE, 2),
        "streamed turn did not commit its assistant message:\n{}",
        tui.screen_text()
    );

    let dsr_queries = tui.output[measure_from..]
        .windows(4)
        .filter(|window| *window == b"\x1b[6n")
        .count();
    let private_dsr_queries = tui.output[measure_from..]
        .windows(5)
        .filter(|window| *window == b"\x1b[?6n")
        .count();
    assert_eq!(
        dsr_queries + private_dsr_queries,
        0,
        "steady-state streamed turn sent {dsr_queries} plain and {private_dsr_queries} \
         private cursor-position (DSR) queries; the renderer must derive the cursor from \
         tracked state instead of a blocking ESC[6n round-trip per commit",
    );

    tui.quit();
}

#[test]
fn tui_pty_streaming_reasoning_body_stays_viewport_only_until_the_gist_commits() {
    // Euler Thinking State design, "streaming" state: while the model
    // reasons, the delta text types out live behind the hairline in the
    // inline viewport. The growing body must NEVER commit to native
    // scrollback row-by-row — after finalize, exactly the ONE-LINE
    // collapsed gist lands in committed history and the body vanishes.
    let temp = tempfile::tempdir().expect("temp dir");
    // Sentence 1 becomes the collapsed gist; the zeta-probe tokens live
    // only in the streamed body and must not survive into the final state.
    let script = write_fixture_script(
        temp.path(),
        "reasoning-stream.json",
        &serde_json::json!({
            "version": 1,
            "responses": [{"events": [
                {"reasoning_delta": "Weighing the residue lemma. "},
                {"sleep_ms": 200},
                {"reasoning_delta": "Cross-checking the modular tower against zeta-probe-alpha and "},
                {"sleep_ms": 200},
                {"reasoning_delta": "zeta-probe-bravo before the contradiction closes."},
                // Observation window: the fully streamed body stays on
                // screen long enough for the harness to see it.
                {"sleep_ms": 2500},
                {"text_delta": "Answer: the lemma holds."},
                {"finished": {"stop_reason": "completed"}},
            ]}]
        })
        .to_string(),
    );
    let script_option = format!("event-script={}", path_str(&script));
    let mut tui = PtyHarness::spawn_with_args(
        temp.path(),
        &[
            "tui",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
        ],
    );
    assert!(tui.wait_for_screen("/ commands"), "{}", tui.screen_text());
    tui.write("prove it\r");

    // The streaming body is visible in the inline viewport while the model
    // reasons, under the live thinking header. A glimpse (not a
    // stable-screen wait): the ~90ms HUD spinner repaints never let the
    // screen go quiet while the transient body is up.
    assert!(
        tui.wait_for_screen_glimpse("zeta-probe-bravo"),
        "streamed reasoning body did not render live:\n{}",
        tui.screen_text()
    );
    let streaming_screen = tui.screen_text();
    assert!(
        streaming_screen.contains("thinking ·"),
        "live thinking header missing during streaming:\n{streaming_screen}"
    );
    // The one-line HUD carries the thinking status and the SOLE esc
    // affordance during the delta phase; the transcript header is the
    // timer alone — the interrupt hint must not be advertised twice.
    assert!(
        streaming_screen.contains("esc to interrupt"),
        "HUD interrupt affordance missing during streaming:\n{streaming_screen}"
    );
    assert!(
        streaming_screen
            .lines()
            .any(|line| line.contains("thinking ·") && !line.contains("esc")),
        "transcript thinking header must carry no esc hint:\n{streaming_screen}"
    );
    assert!(
        !streaming_screen
            .lines()
            .any(|line| line.contains("thinking ·") && line.contains("esc interrupt")),
        "the old transcript-header esc hint is gone:\n{streaming_screen}"
    );

    // Finalize: the answer replaces the body; the thought collapses to the
    // one-line gist.
    assert!(
        tui.wait_for_screen("Answer: the lemma holds."),
        "answer did not render:\n{}",
        tui.screen_text()
    );
    assert!(
        tui.wait_for_screen("thought summary for"),
        "collapsed gist did not render:\n{}",
        tui.screen_text()
    );
    tui.quit();

    // Load-bearing invariant: reconstruct scrollback + screen. The gist
    // line committed exactly once; no intermediate growing-body row leaked
    // into committed history, and the live header hint is gone.
    let final_state = pty_final_state_text(&tui.output, 24, 80);
    let gist_lines = final_state
        .lines()
        .filter(|line| line.contains("Weighing the residue lemma"))
        .count();
    assert_eq!(
        gist_lines, 1,
        "gist must commit exactly once:\n{final_state}"
    );
    let collapsed_lines = final_state
        .lines()
        .filter(|line| line.contains("thought summary for"))
        .count();
    assert_eq!(
        collapsed_lines, 1,
        "collapsed thought header must appear exactly once:\n{final_state}"
    );
    let body_leaks = final_state
        .lines()
        .filter(|line| line.contains("zeta-probe"))
        .count();
    assert_eq!(
        body_leaks, 0,
        "streamed reasoning body leaked into committed history:\n{final_state}"
    );
    assert!(
        !final_state.contains("thinking ·") && !final_state.contains("esc to interrupt"),
        "live thinking header / HUD status leaked into committed history:\n{final_state}"
    );
}

#[test]
#[ignore = "diagnostic: set EULER_RAW_CAPTURE to a raw PTY byte file"]
fn diag_reconstruct_final_state_from_capture() {
    let path = std::env::var("EULER_RAW_CAPTURE").expect("EULER_RAW_CAPTURE");
    let raw = fs::read(path).expect("raw capture");
    let state = pty_final_state_text(&raw, 24, 80);
    println!("=== FINAL STATE ===\n{state}\n=== END ===");
    for needle in [
        "e^(iπ) + 1 = 0",
        "Looking at the repository",
        "Here is an overview",
        "overview please",
    ] {
        let count = state.lines().filter(|line| line.contains(needle)).count();
        println!("COUNT {needle} -> {count}");
    }
}

#[test]
fn tui_pty_session_grant_keeps_tool_blocks_well_formed() {
    // Review v2 §2/§8: after "allow for session", subsequent shell blocks
    // must still render through the block renderer (header + fold), carry
    // the dim `· session grant` tag instead of fresh decision records, and
    // nothing may duplicate.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut responses = Vec::new();
    // `printf` is deliberately NOT statically safe (issue #78): a safe
    // binary like `echo` would auto-approve and never render the panel.
    for (id, cmd) in [
        ("call-1", "printf alpha-one"),
        ("call-2", "printf beta-two"),
        ("call-3", "printf gamma-three"),
    ] {
        responses.push(serde_json::json!({"events": [
            {"tool_call": {"id": id, "name": "run_shell", "input": {"command": cmd}}},
            {"finished": {"stop_reason": "tool_use"}},
        ]}));
    }
    responses.push(serde_json::json!({"events": [
        {"text_delta": "ran all three."},
        {"finished": {"stop_reason": "completed"}},
    ]}));
    let script = write_fixture_script(
        temp.path(),
        "grant-blocks.json",
        &serde_json::json!({"version": 1, "responses": responses}).to_string(),
    );
    let script_option = format!("event-script={}", path_str(&script));
    let mut tui = PtyHarness::spawn_with_args(
        temp.path(),
        &[
            "tui",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
        ],
    );
    assert!(tui.wait_for_screen("/ commands"), "{}", tui.screen_text());
    tui.write("run the three commands\r");
    // First shell call prompts; grant the session scope.
    assert!(
        tui.wait_for_screen("Run command?"),
        "approval panel did not render:\n{}",
        tui.screen_text()
    );
    tui.write("a");
    assert!(
        tui.wait_for_screen("ran all three."),
        "turn did not finish:\n{}",
        tui.screen_text()
    );
    tui.quit();

    let final_state = pty_final_state_text(&tui.output, 24, 80);
    let mut failures = Vec::new();
    for cmd in ["printf alpha-one", "printf beta-two", "printf gamma-three"] {
        let headers = final_state
            .lines()
            .filter(|line| line.contains(&format!("Ran {cmd}")))
            .count();
        if headers != 1 {
            failures.push(format!("`Ran {cmd}` header appears {headers}× (want 1)"));
        }
    }
    for output in ["alpha-one", "beta-two", "gamma-three"] {
        let occurrences = final_state
            .lines()
            .filter(|line| {
                line.contains(output) && !line.contains("Ran ") && !line.contains("run the three")
            })
            .count();
        if occurrences > 1 {
            failures.push(format!("output `{output}` duplicated ({occurrences}×)"));
        }
    }
    // Exactly one decision record; later runs tagged on the header instead.
    let decisions = final_state
        .lines()
        .filter(|line| line.contains("allowed for session"))
        .count();
    if decisions != 1 {
        failures.push(format!("decision records: {decisions} (want 1)"));
    }
    let grant_tags = final_state
        .lines()
        .filter(|line| line.contains("· session grant"))
        .count();
    if grant_tags != 2 {
        failures.push(format!("session-grant header tags: {grant_tags} (want 2)"));
    }
    assert!(
        failures.is_empty(),
        "{}\nFinal state:\n{final_state}",
        failures.join("\n")
    );
}

#[test]
fn tui_pty_resize_drag_never_amplifies_scrollback_copies() {
    // Review v2 §11/§12: a drag (many resize ticks) appended one re-wrapped
    // transcript copy to scrollback per width tick. With commit suspension
    // during resize + item-boundary remap, a drag may leave at most the
    // pre-resize copy plus one bounded partial-item re-emission — never a
    // copy per tick.
    //
    // Issue #38 mechanism under test: intermediate ticks re-render the live
    // viewport only; once the drag settles (450ms trailing debounce), euler
    // runs exactly ONE purge+replay that clears the ENTIRE native scrollback
    // buffer (ESC[2J + ESC[3J) — including any pre-euler content the user's
    // terminal held before euler started — and re-emits euler's own
    // transcript from the event log at the settled width. See
    // docs/contracts/ui.md ("Mouse" section, resize exception) for the full
    // rationale: per-tick append corrupted all three terminals under test
    // (Ghostty, iTerm2, Terminal.app), and no escape/control sequence scopes
    // a scrollback purge to euler-only rows.
    //
    // OWNER-ACCEPTANCE PENDING: this test exercises the mechanism via PTY +
    // vt100 reconstruction only. It does not substitute for hands-on
    // real-terminal dogfood in Ghostty/iTerm2/Terminal.app, which remains
    // the outstanding #38 acceptance step before this trade-off is
    // considered fully settled.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut events = Vec::new();
    for paragraph in 1..=6 {
        let sentence = format!(
            "Paragraph {paragraph}: content long enough to wrap and scroll so \
             a resize drag has committed rows above the viewport to corrupt."
        );
        for chunk in sentence.as_bytes().chunks(8) {
            events.push(serde_json::json!({
                "text_delta": String::from_utf8_lossy(chunk)
            }));
        }
        events.push(serde_json::json!({"text_delta": "\n\n"}));
    }
    events.push(serde_json::json!({"finished": {"stop_reason": "completed"}}));
    let script = write_fixture_script(
        temp.path(),
        "drag-stream.json",
        &serde_json::json!({"version": 1, "responses": [{"events": events}]}).to_string(),
    );
    let script_option = format!("event-script={}", path_str(&script));
    let mut tui = PtyHarness::spawn_with_args(
        temp.path(),
        &[
            "tui",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
        ],
    );
    assert!(tui.wait_for_screen("/ commands"), "{}", tui.screen_text());
    tui.write("overview please\r");
    assert!(tui.wait_for_screen("Paragraph 6:"), "{}", tui.screen_text());
    // Simulate a drag: many rapid width ticks in both directions.
    for cols in [96, 92, 88, 84, 80, 76, 72, 76, 82, 90, 100] {
        tui.resize(24, cols);
        std::thread::sleep(Duration::from_millis(30));
    }
    // Quiescence so the debounced settled-width replay runs before quitting.
    std::thread::sleep(Duration::from_millis(900));
    tui.quit();

    // Assert the MECHANISM from the raw byte stream (emulator-independent):
    // scrollback (bridge) emissions must not happen per drag tick. Content
    // committed before the drag stays put; after quiescence at most one
    // bounded re-emission may occur.
    let drag_start = tui.resizes.first().map(|(offset, _, _)| *offset).unwrap();
    let drag_end = tui.resizes.last().map(|(offset, _, _)| *offset).unwrap();
    let during: Vec<String> = extract_bridge_committed_rows(&tui.output[drag_start..drag_end]);
    assert!(
        during.is_empty(),
        "bridge emissions during the drag ({}) — per-tick amplification:\n{}",
        during.len(),
        during.join("\n")
    );
    // Invariant (issue #38): a re-emission is only legal when a scrollback
    // purge precedes it — append-without-purge is the fossil-copy bug. Every
    // paragraph may appear at most once more than the number of purges
    // (the original commit + one purge-paired re-emission per purge), and
    // the final post-purge state must hold exactly one copy.
    let purge_count = tui
        .output
        .windows(4)
        .filter(|window| *window == b"\x1b[3J")
        .count();
    assert!(
        purge_count >= 1,
        "no scrollback purge observed — the debounced settled-width replay did not run"
    );
    let all_rows = extract_bridge_committed_rows(&tui.output);
    let mut failures = Vec::new();
    for paragraph in 1..=6 {
        let needle = format!("Paragraph {paragraph}:");
        let occurrences = all_rows.iter().filter(|row| row.contains(&needle)).count();
        if occurrences > purge_count + 1 {
            failures.push(format!(
                "`{needle}` bridge-committed {occurrences}× with only {purge_count} purges — an append happened without a purge"
            ));
        }
    }
    let last_purge = tui
        .output
        .windows(4)
        .rposition(|window| window == b"\x1b[3J")
        .map(|at| at + 4)
        .expect("purge offset");
    let settled_rows = extract_bridge_committed_rows(&tui.output[last_purge..]);
    for paragraph in 1..=6 {
        let needle = format!("Paragraph {paragraph}:");
        let occurrences = settled_rows
            .iter()
            .filter(|row| row.contains(&needle))
            .count();
        if occurrences > 1 {
            failures.push(format!(
                "`{needle}` appears {occurrences}× after the final purge (want exactly one settled copy)"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{}\nBridge rows:\n{}",
        failures.join("\n"),
        all_rows.join("\n")
    );
}

/// Streams `count` wrapping paragraphs plus a finished event into a fixture
/// script and returns the `--provider-option` value for it.
fn wrapping_paragraph_script(dir: &Path, name: &str, count: usize) -> String {
    let mut events = Vec::new();
    for paragraph in 1..=count {
        let sentence = format!(
            "Paragraph {paragraph}: content long enough to wrap and scroll so \
             history rows land in native scrollback before the repaint."
        );
        for chunk in sentence.as_bytes().chunks(8) {
            events.push(serde_json::json!({
                "text_delta": String::from_utf8_lossy(chunk)
            }));
        }
        events.push(serde_json::json!({"text_delta": "\n\n"}));
    }
    events.push(serde_json::json!({"finished": {"stop_reason": "completed"}}));
    let script = write_fixture_script(
        dir,
        name,
        &serde_json::json!({"version": 1, "responses": [{"events": events}]}).to_string(),
    );
    format!("event-script={}", path_str(&script))
}

#[test]
fn tui_pty_theme_switch_replay_keeps_history_head_reachable() {
    // Resize/repaint dogfood repro 1: switching themes runs a purge+replay;
    // everything that was above the fold (banner, greeting, first user
    // message) must still exist in the emulator's scrollback afterwards.
    // The old replay re-committed the head rows through the scroll-region
    // bridge, which scrolled BLANK rows (the screen had just been cleared)
    // into scrollback while the viewport draw overpainted the rows the
    // bridge wrote — the head of the session became an unreachable void.
    let temp = tempfile::tempdir().expect("temp dir");
    let script_option = wrapping_paragraph_script(temp.path(), "theme-replay.json", 6);
    let mut tui = PtyHarness::spawn_with_args(
        temp.path(),
        &[
            "tui",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
        ],
    );
    assert!(tui.wait_for_screen("/ commands"), "{}", tui.screen_text());
    tui.write("overview please\r");
    assert!(tui.wait_for_screen("Paragraph 6:"), "{}", tui.screen_text());

    tui.write("/theme light\r");
    assert!(
        tui.wait_for_screen("theme set to"),
        "theme switch did not run:\n{}",
        tui.screen_text()
    );
    std::thread::sleep(Duration::from_millis(300));
    tui.quit();

    let state = pty_emulator_final_state(&tui.output, (24, 80), &[]);
    let mut failures = Vec::new();
    for needle in ["e^(iπ) + 1 = 0", "overview please"] {
        let occurrences = state.lines().filter(|line| line.contains(needle)).count();
        if occurrences != 1 {
            failures.push(format!(
                "`{needle}` reachable {occurrences}× after theme replay (want 1)"
            ));
        }
    }
    for paragraph in 1..=6 {
        let needle = format!("Paragraph {paragraph}:");
        let occurrences = state.lines().filter(|line| line.contains(&needle)).count();
        if occurrences != 1 {
            failures.push(format!(
                "`{needle}` reachable {occurrences}× after theme replay (want 1)"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "theme replay destroyed or duplicated history:\n{}\nFinal emulator state:\n{state}",
        failures.join("\n")
    );
}

#[test]
fn tui_pty_grow_settles_top_anchored_with_nothing_below_footer() {
    // Resize/repaint dogfood repro 3: grow the window mid-session. After the
    // settled replay: content that fits the taller screen is top-anchored,
    // the footer is the LAST painted row (no stray transcript below it), and
    // no line of the session exists twice. The old code clamped the viewport
    // to the startup height and re-committed head rows through the
    // scroll-region bridge, which painted a degraded duplicate of the
    // session head BELOW the live footer.
    let temp = tempfile::tempdir().expect("temp dir");
    let script_option = wrapping_paragraph_script(temp.path(), "grow-settle.json", 8);
    let mut tui = PtyHarness::spawn_with_args(
        temp.path(),
        &[
            "tui",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
        ],
    );
    assert!(tui.wait_for_screen("/ commands"), "{}", tui.screen_text());
    tui.write("overview please\r");
    assert!(tui.wait_for_screen("Paragraph 8:"), "{}", tui.screen_text());

    // Grow both dimensions well past the startup size; PTY resize delivery
    // is slow (~300ms ticks), so allow the 450ms debounce to settle after.
    tui.resize(60, 132);
    std::thread::sleep(Duration::from_millis(1200));
    // Harvest the settled frame and remember where it ends: the quit path
    // prints the exit recap BELOW the app frame by design, which must not
    // count against the "nothing below the footer" assertion.
    assert!(tui.wait_for_screen("/ commands"), "{}", tui.screen_text());
    let settled_len = tui.output.len();
    tui.quit();

    let resizes = tui.resizes.clone();
    let state = pty_emulator_final_state(&tui.output[..settled_len], (24, 80), &resizes);
    let mut failures = Vec::new();
    for paragraph in 1..=8 {
        let needle = format!("Paragraph {paragraph}:");
        let occurrences = state.lines().filter(|line| line.contains(&needle)).count();
        if occurrences != 1 {
            failures.push(format!(
                "`{needle}` present {occurrences}× after settled grow (want 1)"
            ));
        }
    }
    let banner = state
        .lines()
        .filter(|line| line.contains("e^(iπ) + 1 = 0"))
        .count();
    if banner != 1 {
        failures.push(format!("banner caption present {banner}× (want 1)"));
    }

    let screen = pty_final_screen_rows(&tui.output[..settled_len], (24, 80), &resizes);
    // At 60 rows the whole session fits: the banner must be on the visible
    // screen, top-anchored (the old stale-height clamp left it in scrollback
    // or duplicated below the footer).
    let banner_row = screen.iter().position(|row| row.contains("e^(iπ) + 1 = 0"));
    // The caption is the 7th banner row; top-anchored content puts it in the
    // top handful of screen rows.
    match banner_row {
        None => failures.push("banner caption not on the settled screen".to_owned()),
        Some(row) if row > 8 => {
            failures.push(format!(
                "banner caption at screen row {row} — content is not top-anchored"
            ));
        }
        Some(_) => {}
    }
    // The bottom chrome (composer rail then status line) is the LAST painted
    // content: no transcript row may appear below it (the old code painted a
    // degraded copy of the session head below the live footer after a grow).
    match screen
        .iter()
        .rposition(|row| row.trim_start().starts_with('▌'))
    {
        None => failures.push("composer rail not on the settled screen".to_owned()),
        Some(composer_row) => {
            for (offset, row) in screen[composer_row + 1..].iter().enumerate() {
                let trimmed = row.trim();
                let is_status_row = trimmed.contains(" · ");
                if !trimmed.is_empty() && !is_status_row {
                    failures.push(format!(
                        "non-chrome row {} below the composer: {row:?}",
                        composer_row + 1 + offset
                    ));
                }
                if trimmed.contains("Paragraph") || trimmed.contains("e^(iπ)") {
                    failures.push(format!(
                        "transcript content below the composer at row {}: {row:?}",
                        composer_row + 1 + offset
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "settled grow left a corrupted frame:\n{}\nScreen:\n{}\nEmulator state:\n{state}",
        failures.join("\n"),
        screen.join("\n")
    );
}

#[test]
fn tui_pty_fold_toggle_replay_after_resize_keeps_history_intact() {
    // Resize/repaint dogfood repros 2/4/5: a ctrl+o fold toggle triggers a
    // purge+replay. Toggling right after a resize (before the debounced
    // settled replay has run) must not consume stale geometry, and the
    // toggle's own replay must not turn the rows above the fold into a
    // void. Covers: expand -> collapse in a stable window, and resize ->
    // immediate toggle as the first repaint after the size change.
    let temp = tempfile::tempdir().expect("temp dir");
    let tool_output = "for i in $(seq 1 30); do echo tool-line-$i; done";
    let responses = serde_json::json!({"version": 1, "responses": [
        {"events": [
            {"tool_call": {"id": "call-1", "name": "run_shell", "input": {"command": tool_output}}},
            {"finished": {"stop_reason": "tool_use"}},
        ]},
        {"events": [
            {"text_delta": "Ran the generator; thirty lines of output captured for the fold.\n\n"},
            {"text_delta": "Summary paragraph one long enough to wrap at the narrowed width and keep the collapsed transcript taller than the screen.\n\n"},
            {"text_delta": "Summary paragraph two long enough to wrap at the narrowed width and keep the collapsed transcript taller than the screen.\n\n"},
            {"text_delta": "Summary paragraph three long enough to wrap at the narrowed width and keep the collapsed transcript taller than the screen."},
            {"finished": {"stop_reason": "completed"}},
        ]},
    ]});
    let script = write_fixture_script(temp.path(), "fold-replay.json", &responses.to_string());
    let script_option = format!("event-script={}", path_str(&script));
    let mut tui = PtyHarness::spawn_with_args(
        temp.path(),
        &[
            "tui",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
        ],
    );
    assert!(tui.wait_for_screen("/ commands"), "{}", tui.screen_text());
    tui.write("generate lines\r");
    assert!(
        tui.wait_for_screen("Run command?"),
        "approval panel did not render:\n{}",
        tui.screen_text()
    );
    tui.write("a");
    assert!(
        tui.wait_for_screen("thirty lines of output captured"),
        "turn did not finish:\n{}",
        tui.screen_text()
    );
    assert!(
        tui.wait_for_screen("25 more lines · ctrl+o expand"),
        "fold affordance missing:\n{}",
        tui.screen_text()
    );

    // Expand (replay 1): hidden middle output rows become visible.
    tui.write("\x0f");
    assert!(
        tui.wait_for_screen("tool-line-15"),
        "expand did not reveal folded output:\n{}",
        tui.screen_text()
    );
    // Collapse (replay 2) in a stable window — repro 2's every-time case.
    tui.write("\x0f");
    assert!(
        tui.wait_for_screen("25 more lines · ctrl+o expand"),
        "collapse did not restore the fold affordance:\n{}",
        tui.screen_text()
    );

    // Narrow the window and IMMEDIATELY toggle: the toggle's replay is the
    // first repaint after the size change (repro 4). Then let the debounced
    // settled replay run too.
    tui.resize(24, 72);
    tui.write("\x0f");
    assert!(
        tui.wait_for_screen("tool-line-15"),
        "post-resize expand did not reveal folded output:\n{}",
        tui.screen_text()
    );
    std::thread::sleep(Duration::from_millis(1200));
    // Final collapse so the settled state is compact: the vt100 test
    // emulator cannot page scrollback deeper than one screen height, and
    // the void being asserted on is exactly the head of the transcript.
    tui.write("\x0f");
    assert!(
        tui.wait_for_screen("25 more lines · ctrl+o expand"),
        "final collapse did not restore the fold affordance:\n{}",
        tui.screen_text()
    );
    std::thread::sleep(Duration::from_millis(300));
    tui.quit();

    let resizes = tui.resizes.clone();
    let state = pty_emulator_final_state(&tui.output, (24, 80), &resizes);
    let mut failures = Vec::new();
    for needle in [
        "e^(iπ) + 1 = 0",
        "generate lines",
        "thirty lines of output captured",
        "more lines · ctrl+o expand",
    ] {
        let occurrences = state.lines().filter(|line| line.contains(needle)).count();
        if occurrences != 1 {
            failures.push(format!(
                "`{needle}` reachable {occurrences}× after fold replays (want 1)"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "fold-toggle replays corrupted history:\n{}\nFinal emulator state:\n{state}",
        failures.join("\n")
    );
}

#[test]
fn tui_pty_transcript_lines_commit_exactly_once() {
    // Regression for the duplicate-line streaming repaint bug (Warm Spine
    // implementation review, P1): the final terminal state — scrollback plus
    // visible screen — must contain each transcript line exactly once, even
    // when a long response streams in small deltas and scrolls the viewport.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut events = Vec::new();
    for paragraph in 1..=6 {
        let sentence = format!(
            "Paragraph {paragraph}: here is an overview sentence that is long \
             enough to wrap at eighty columns and scroll the viewport as it \
             streams in small deltas."
        );
        for chunk in sentence.as_bytes().chunks(8) {
            events.push(serde_json::json!({
                "text_delta": String::from_utf8_lossy(chunk)
            }));
        }
        events.push(serde_json::json!({"text_delta": "\n\n"}));
    }
    events.push(serde_json::json!({"finished": {"stop_reason": "completed"}}));
    let script = write_fixture_script(
        temp.path(),
        "long-stream.json",
        &serde_json::json!({"version": 1, "responses": [{"events": events}]}).to_string(),
    );
    let script_option = format!("event-script={}", path_str(&script));
    let mut tui = PtyHarness::spawn_with_args(
        temp.path(),
        &[
            "tui",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
        ],
    );
    assert!(
        tui.wait_for_screen("/ commands"),
        "initial TUI did not render:\n{}",
        tui.screen_text()
    );
    tui.write("overview please\r");
    assert!(
        tui.wait_for_screen("Paragraph 6:"),
        "streamed response did not render:\n{}",
        tui.screen_text()
    );
    tui.quit();

    let final_state = pty_final_state_text(&tui.output, 24, 80);
    let mut failures = Vec::new();
    for paragraph in 1..=6 {
        let needle = format!("Paragraph {paragraph}:");
        let occurrences = final_state
            .lines()
            .filter(|line| line.contains(&needle))
            .count();
        if occurrences != 1 {
            failures.push(format!("`{needle}` committed {occurrences}× (expected 1)"));
        }
    }
    let banner_caption = final_state
        .lines()
        .filter(|line| line.contains("e^(iπ) + 1 = 0"))
        .count();
    if banner_caption != 1 {
        failures.push(format!(
            "banner caption committed {banner_caption}× (expected 1)"
        ));
    }
    assert!(
        failures.is_empty(),
        "duplicate-line repaint bug:\n{}\nFinal state:\n{final_state}",
        failures.join("\n")
    );
}

#[test]
fn tui_pty_submit_fixture_turn_and_quit() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("tui-events.jsonl");
    let mut tui = PtyHarness::spawn_with_args(
        temp.path(),
        &["--provider", "fixture", "--provenance", path_str(&log)],
    );

    assert!(
        tui.wait_for_screen("echo(medium) · ctx"),
        "initial TUI did not render:\n{}",
        tui.screen_text()
    );
    tui.write("hello pty\r");
    assert!(
        tui.wait_for_screen("user: hello pty"),
        "fixture response did not render:\n{}",
        tui.screen_text()
    );

    tui.quit();
}

#[test]
fn tui_pty_quit_during_turn_unwinds_and_releases_session_lock() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = isolated_home();
    let log = temp.path().join("tui-events.jsonl");
    let script = write_fixture_script(
        temp.path(),
        "slow-turn.json",
        r#"{
  "version": 1,
  "responses": [
    {
      "events": [
        { "sleep_ms": 5000 },
        { "sleep_ms": 5000 },
        { "text_delta": "too late" },
        { "finished": { "stop_reason": "completed" } }
      ]
    }
  ]
}
"#,
    );
    let script_option = format!("event-script={}", path_str(&script));
    let mut tui = PtyHarness::spawn_with_args(
        temp.path(),
        &[
            "tui",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
            "--provenance",
            path_str(&log),
        ],
    );

    assert!(
        tui.wait_for_screen("echo(medium) · ctx"),
        "{}",
        tui.screen_text()
    );
    tui.write("slow turn\r");
    assert!(
        tui.wait_for_screen_glimpse("esc to interrupt"),
        "turn never entered flight:\n{}",
        tui.screen_text()
    );
    let quit_started = Instant::now();
    tui.write("/quit\r");
    assert!(
        tui.wait_success(),
        "TUI did not unwind after in-flight quit:\n{}",
        tui.screen_text()
    );
    // The scripted turn takes 10s; an unwind well under that proves /quit
    // did not wait for it. The margin absorbs loaded-runner scheduling
    // (issue #145 family) while staying far from the 10s ceiling.
    assert!(
        quit_started.elapsed() < Duration::from_secs(8),
        "/quit waited for the scripted provider response instead of interrupting the turn"
    );
    assert!(
        !tui.screen_text().contains("too late"),
        "provider output arrived after /quit:\n{}",
        tui.screen_text()
    );

    let retried = command_with_home(env!("CARGO_BIN_EXE_euler"), &home)
        .arg("--provider")
        .arg("fixture")
        .arg("--provenance")
        .arg(&log)
        .stdin(Stdio::null())
        .output()
        .expect("retry after TUI unwind");
    assert!(
        retried.status.success(),
        "normal TUI unwind should release the advisory lock: {}",
        String::from_utf8_lossy(&retried.stderr)
    );
}

#[cfg(unix)]
#[test]
fn fresh_tui_runs_a_persistently_enabled_linked_process() {
    let home = isolated_home();
    let extension_dir = tempfile::tempdir().expect("extension dir");
    let sdk_source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../python/euler_managed_process_sdk/src");
    write_managed_process_extension_manifest(
        extension_dir.path(),
        "python-fresh-tui",
        "0.1.1",
        &[
            "python3".to_owned(),
            "-B".to_owned(),
            "-u".to_owned(),
            "extension.py".to_owned(),
        ],
    );
    let manifest_path = extension_dir
        .path()
        .join(euler_core::EXTENSION_MANIFEST_FILE);
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("read manifest"))
            .expect("manifest json");
    manifest["capabilities"] = serde_json::json!([]);
    manifest["commands"][0]["required_capabilities"] = serde_json::json!([]);
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write capability-free manifest");
    fs::write(
        extension_dir.path().join("extension.py"),
        format!(
            "import sys\nsys.path.insert(0, {sdk_source:?})\nfrom euler_managed_process_sdk import serve\nserve({{'inspect': lambda context: {{'fresh_tui': True}}}})\n",
            sdk_source = sdk_source.to_string_lossy()
        ),
    )
    .expect("write Python extension");
    configure_linked_extension(
        env!("CARGO_BIN_EXE_euler"),
        &home,
        extension_dir.path(),
        "python-fresh-tui",
    );

    let mut tui = PtyHarness::spawn_with_args(home.path(), &["tui", "--provider", "fixture"]);
    assert!(tui.wait_for_screen("echo(medium) · ctx"));
    tui.write("/extension run python-fresh-tui.inspect {}\r");
    assert!(
        tui.wait_for_screen("fresh_tui"),
        "persistently enabled linked command did not run:\n{}",
        tui.screen_text()
    );
    tui.quit();
}

#[cfg(unix)]
#[test]
fn tty_line_mode_denies_linked_capabilities_before_process_launch() {
    let home = isolated_home();
    let extension_dir = tempfile::tempdir().expect("extension dir");
    write_managed_process_extension_manifest(
        extension_dir.path(),
        "python-tty-permission",
        "0.1.1",
        &[
            "python3".to_owned(),
            "-B".to_owned(),
            "-u".to_owned(),
            "extension.py".to_owned(),
        ],
    );
    fs::write(
        extension_dir.path().join("extension.py"),
        "from pathlib import Path\nPath('invoked').write_text('yes')\n",
    )
    .expect("write marker process");
    configure_linked_extension(
        env!("CARGO_BIN_EXE_euler"),
        &home,
        extension_dir.path(),
        "python-tty-permission",
    );

    let mut line = PtyHarness::spawn_with_args(home.path(), &["--no-tty", "--provider", "fixture"]);
    line.write("extension_run python-tty-permission.inspect {}\r");
    assert!(
        line.wait_for_screen("permission: allow provenance-read, artifact-write"),
        "TTY line mode did not ask for capability approval:\n{}",
        line.screen_text()
    );
    line.write("n\r");
    assert!(line.wait_for_screen("capability denied"));
    line.write("exit\r");
    assert!(line.wait_success());
    assert!(
        !extension_dir.path().join("invoked").exists(),
        "managed process launched before capability approval"
    );
}

#[test]
fn tui_pty_without_provenance_writes_home_session_store() {
    let home = isolated_home();
    let mut tui = PtyHarness::spawn_with_args(home.path(), &["tui", "--provider", "fixture"]);

    assert!(
        tui.wait_for_screen("echo(medium) · ctx"),
        "initial TUI did not render:\n{}",
        tui.screen_text()
    );
    tui.write("hello tui home\r");
    assert!(
        tui.wait_for_screen("user: hello tui home"),
        "fixture response did not render:\n{}",
        tui.screen_text()
    );

    tui.quit();

    let session_id = only_home_session_id(home.path());
    let events = read_jsonl(&home_session_log(home.path(), &session_id));
    assert_eq!(events.len(), 7);
    assert!(events.iter().all(|event| event.session == session_id));
}

#[test]
fn tui_name_session_updates_resume_picker_label() {
    let home = isolated_home();
    let mut tui = PtyHarness::spawn_with_args(home.path(), &["tui", "--provider", "fixture"]);
    assert!(
        tui.wait_for_screen("echo(medium) · ctx"),
        "initial TUI did not render:\n{}",
        tui.screen_text()
    );

    tui.write("/name dogfood session\r");
    assert!(
        tui.wait_for_screen("session named dogfood session"),
        "name confirmation did not render:\n{}",
        tui.screen_text()
    );
    tui.quit();

    let session_id = only_home_session_id(home.path());
    let metadata_path = home
        .path()
        .join(".euler")
        .join("sessions")
        .join(&session_id)
        .join("session.json");
    let metadata: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(metadata_path).expect("session metadata"))
            .expect("metadata json");
    assert_eq!(
        metadata.get("name").and_then(serde_json::Value::as_str),
        Some("dogfood session")
    );

    let mut resumed = PtyHarness::spawn_with_args(home.path(), &["tui", "--provider", "fixture"]);
    assert!(resumed.wait_for_screen("echo(medium) · ctx"));
    resumed.write("/resume\r");
    assert!(
        resumed.wait_for_screen("dogfood session"),
        "resume picker did not show named session:\n{}",
        resumed.screen_text()
    );
    resumed.write("\x1b[B\r");
    assert!(
        resumed.wait_for_screen("resumed session dogfood session"),
        "resume notice did not use named session label:\n{}",
        resumed.screen_text()
    );
    resumed.quit();
}

#[test]
fn tui_resume_picker_lists_home_sessions() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let mut first = command_with_home(exe, &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn first euler");
    first
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"saved for picker\n")
        .expect("write stdin");
    assert!(first
        .wait_with_output()
        .expect("wait first")
        .status
        .success());
    let saved_id = only_home_session_id(home.path());

    let mut tui = PtyHarness::spawn_with_args(home.path(), &["tui", "--provider", "fixture"]);
    assert!(tui.wait_for_screen("echo(medium) · ctx"));
    tui.write("/resume\r");
    assert!(
        tui.wait_for_screen("saved for picker"),
        "resume picker did not list derived title:\n{}",
        tui.screen_text()
    );
    assert!(
        tui.wait_for_screen(&saved_id),
        "resume picker did not list saved id {saved_id}:\n{}",
        tui.screen_text()
    );
    tui.write("\x1b[B\r");
    assert!(
        tui.wait_for_screen("resumed session saved for picker"),
        "resume selection did not resume saved title (id {saved_id}):\n{}",
        tui.screen_text()
    );
    assert!(
        tui.wait_for_screen("events replayed · model context folded to stubs"),
        "resume boundary divider missing:\n{}",
        tui.screen_text()
    );
    assert!(
        tui.wait_for_screen("user: saved for picker"),
        "resumed transcript was not rendered:\n{}",
        tui.screen_text()
    );
    tui.write("after tui resume\r");
    // Fixture echo concatenates prior turns; the 9-cell ledger gutter can wrap
    // the long assistant line, so match stable prefixes rather than one span.
    assert!(
        tui.wait_for_screen("assistant: user: saved for picker"),
        "post-resume turn did not render:\n{}",
        tui.screen_text()
    );
    assert!(
        tui.wait_for_screen("after tui"),
        "post-resume user text did not render:\n{}",
        tui.screen_text()
    );
    tui.quit();

    let events = read_jsonl(&home_session_log(home.path(), &saved_id));
    assert!(events.iter().all(|event| event.session == saved_id));
    let replayed =
        replay_transcript_with_home(exe, home.path(), &home_session_log(home.path(), &saved_id));
    assert!(replayed.contains("user: saved for picker\n"));
    assert!(replayed.contains("user: after tui resume\n"));
}

#[test]
fn bare_resume_in_pty_opens_tui_with_restored_transcript() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let mut first = command_with_home(exe, &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn first euler");
    first
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"direct resume history\n")
        .expect("write stdin");
    assert!(first
        .wait_with_output()
        .expect("wait first")
        .status
        .success());
    let session_id = only_home_session_id(home.path());
    append_session_rename_event(
        &home_session_log(home.path(), &session_id),
        &session_id,
        "direct resume named",
    );

    let mut resumed = PtyHarness::spawn_with_args(
        home.path(),
        &["--resume", &session_id, "--provider", "fixture"],
    );
    assert!(
        resumed.wait_for_screen("resumed session direct resume named"),
        "direct resume did not enter restored TUI:\n{}",
        resumed.screen_text()
    );
    assert!(
        resumed.wait_for_screen("user: direct resume history"),
        "direct resume did not restore transcript:\n{}",
        resumed.screen_text()
    );
    assert!(
        resumed.wait_for_screen("direct resume named"),
        "direct resume did not restore session display metadata:\n{}",
        resumed.screen_text()
    );
    assert!(
        !resumed
            .screen_text()
            .contains("each input line is sent as a separate turn"),
        "direct resume entered line-oriented mode:\n{}",
        resumed.screen_text()
    );
    resumed.write("direct resume followup\r");
    assert!(
        resumed.wait_for_screen("direct resume followup"),
        "resumed TUI did not accept a follow-up:\n{}",
        resumed.screen_text()
    );
    resumed.quit();

    let events = read_jsonl(&home_session_log(home.path(), &session_id));
    assert!(events.iter().all(|event| event.session == session_id));
    assert!(events.iter().any(|event| {
        event.kind.as_str() == EventKind::USER_MESSAGE
            && event
                .payload
                .get("content")
                .and_then(serde_json::Value::as_str)
                == Some("direct resume followup")
    }));
}

#[test]
fn bare_resume_in_pty_uses_derived_title_without_name() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let mut first = command_with_home(exe, &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn first euler");
    first
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"derived resume title\n")
        .expect("write stdin");
    assert!(first
        .wait_with_output()
        .expect("wait first")
        .status
        .success());
    let session_id = only_home_session_id(home.path());

    let mut resumed = PtyHarness::spawn_with_args(
        home.path(),
        &["--resume", &session_id, "--provider", "fixture"],
    );
    assert!(
        resumed.wait_for_screen("resumed session derived resume title"),
        "direct resume did not use the derived session title:\n{}",
        resumed.screen_text()
    );
    resumed.quit();
}

#[test]
fn explicit_tui_resume_in_pty_restores_transcript() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let mut first = command_with_home(exe, &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn first euler");
    first
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"explicit tui history\n")
        .expect("write stdin");
    assert!(first
        .wait_with_output()
        .expect("wait first")
        .status
        .success());
    let session_id = only_home_session_id(home.path());

    let mut resumed = PtyHarness::spawn_with_args(
        home.path(),
        &["tui", "--resume", &session_id, "--provider", "fixture"],
    );
    assert!(
        resumed.wait_for_screen("user: explicit tui history"),
        "explicit TUI resume did not restore transcript:\n{}",
        resumed.screen_text()
    );
    resumed.quit();
}

#[test]
fn explicit_run_resume_in_pty_stays_line_oriented() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let first = command_with_home(exe, &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(b"explicit run history\n")?;
            child.wait_with_output()
        })
        .expect("create saved session");
    assert!(first.status.success());
    let session_id = only_home_session_id(home.path());

    let mut resumed = PtyHarness::spawn_with_args(
        home.path(),
        &["run", "--resume", &session_id, "--provider", "fixture"],
    );
    assert!(
        resumed.wait_for_screen("each input line is sent as a separate turn"),
        "explicit run resume did not stay line-oriented:\n{}",
        resumed.screen_text()
    );
    assert!(
        !resumed.screen_text().contains("echo(medium) · ctx"),
        "explicit run resume unexpectedly entered TUI:\n{}",
        resumed.screen_text()
    );
    resumed.write("exit\r");
    assert!(resumed.wait_success());
}

#[test]
fn closed_session_scrub_reads_exact_value_from_stdin_and_appends_audit() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let euler_home = euler_core::EulerHome::from_root(home.path().join(".euler")).expect("home");
    let store = euler_core::SessionStore::new(euler_home).expect("store");
    let record = store.create_session().expect("session");
    let secret = "  closed-session-secret-1234  ";
    let event = EventEnvelope::new(
        record.id(),
        "root",
        None,
        EventKind::TOOL_CALL,
        object([(
            "input",
            serde_json::json!({"command": format!("echo {secret}")}),
        )]),
    );
    let writer = euler_core::ProvenanceWriter::new(record.events_path()).expect("writer");
    writer.append(&[event]).expect("append");
    drop(writer);

    let mut child = command_with_home(exe, &home)
        .arg("scrub")
        .arg(record.id())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn scrub");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(format!("{secret}\n").as_bytes())
        .expect("write secret");
    let output = child.wait_with_output().expect("wait scrub");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output
        .stdout
        .windows(secret.len())
        .any(|bytes| bytes == secret.as_bytes()));
    let raw = fs::read_to_string(record.events_path()).expect("events");
    assert!(!raw.contains(secret));
    assert!(raw.contains(EventKind::SECRET_SCRUBBED));
}

const BLOB_HASH: &str = "bef57ec7f53a6d40beb640a780a639c83bc29ac8a9816f1fc6c5c6dcd93c4721";
const ANTHROPIC_API_KEY_SENTINEL: &str = "euler-secret-boundary-anthropic-api-key-2315";
const OPENAI_API_KEY_SENTINEL: &str = "euler-secret-boundary-openai-api-key-2315";
const OPENROUTER_API_KEY_SENTINEL: &str = "euler-secret-boundary-openrouter-api-key-2315";
const XAI_API_KEY_SENTINEL: &str = "euler-secret-boundary-xai-api-key-2315";
const EULER_AUTH_FILE_PATH_SENTINEL: &str = "euler-secret-boundary-auth-file-path-2315";
const EULER_CUSTOM_API_KEY_SENTINEL: &str = "euler-secret-boundary-custom-api-key-2315";
const AWS_SECRET_ACCESS_KEY_SENTINEL: &str = "euler-secret-boundary-aws-secret-access-key-2315";
const EULER_TEST_TOKEN_SENTINEL: &str = "euler-secret-boundary-test-token-2315";
const EULER_TEST_SECRET_SENTINEL: &str = "euler-secret-boundary-test-secret-2315";
const EULER_AUTH_FILE_CONTENT_SENTINEL: &str = "euler-secret-boundary-auth-file-content-2315";
#[derive(Debug)]
struct SecretSentinel {
    label: &'static str,
    value: Vec<u8>,
}

#[derive(Debug)]
struct SecretFixture {
    auth_file: std::path::PathBuf,
    sentinels: Vec<SecretSentinel>,
}

impl SecretFixture {
    fn new(home: &Path) -> Self {
        let auth_file = home.join(EULER_AUTH_FILE_PATH_SENTINEL).join("auth.json");
        fs::create_dir_all(auth_file.parent().expect("auth file parent"))
            .expect("create auth file parent");
        let auth_file_content = format!("{EULER_AUTH_FILE_CONTENT_SENTINEL}\n");
        fs::write(&auth_file, &auth_file_content).expect("write auth file");
        let auth_contents = fs::read_to_string(&auth_file).expect("read auth file");
        assert!(auth_contents.contains(EULER_AUTH_FILE_CONTENT_SENTINEL));

        Self {
            sentinels: vec![
                SecretSentinel::new("ANTHROPIC_API_KEY", ANTHROPIC_API_KEY_SENTINEL),
                SecretSentinel::new("OPENAI_API_KEY", OPENAI_API_KEY_SENTINEL),
                SecretSentinel::new("OPENROUTER_API_KEY", OPENROUTER_API_KEY_SENTINEL),
                SecretSentinel::new("XAI_API_KEY", XAI_API_KEY_SENTINEL),
                SecretSentinel::new("EULER_AUTH_FILE", path_str(&auth_file)),
                SecretSentinel::new("EULER_AUTH_FILE path marker", EULER_AUTH_FILE_PATH_SENTINEL),
                SecretSentinel::new("EULER_CUSTOM_API_KEY", EULER_CUSTOM_API_KEY_SENTINEL),
                SecretSentinel::new("AWS_SECRET_ACCESS_KEY", AWS_SECRET_ACCESS_KEY_SENTINEL),
                SecretSentinel::new("EULER_TEST_TOKEN", EULER_TEST_TOKEN_SENTINEL),
                SecretSentinel::new("EULER_TEST_SECRET", EULER_TEST_SECRET_SENTINEL),
                SecretSentinel::new("EULER_AUTH_FILE content", EULER_AUTH_FILE_CONTENT_SENTINEL),
                SecretSentinel::new("EULER_AUTH_FILE content newline", &auth_file_content),
            ],
            auth_file,
        }
    }
}

impl SecretSentinel {
    fn new(label: &'static str, value: &str) -> Self {
        Self {
            label,
            value: value.as_bytes().to_vec(),
        }
    }
}

struct PtyHarness {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    rx: Receiver<Vec<u8>>,
    output: Vec<u8>,
    cursor_report_scan_offset: usize,
    master: Box<dyn portable_pty::MasterPty + Send>,
    resizes: Vec<(usize, u16, u16)>,
}

const PTY_POLL_INTERVAL: Duration = Duration::from_millis(20);
const PTY_QUIET_INTERVAL: Duration = Duration::from_millis(100);

impl PtyHarness {
    fn spawn_with_args(home: &Path, args: &[&str]) -> Self {
        Self::spawn_with_args_in_dir(home, None, args)
    }

    /// Like `spawn_with_args` but runs the process in `workspace` when given,
    /// so project-context discovery scans a controlled folder rather than the
    /// test's own CWD.
    fn spawn_with_args_in_dir(home: &Path, workspace: Option<&Path>, args: &[&str]) -> Self {
        record_recent_catalog_refresh_for_test(home);
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_euler"));
        cmd.args(args);
        cmd.env("HOME", home.as_os_str());
        if let Some(workspace) = workspace {
            cmd.cwd(workspace.as_os_str());
        }

        let child = pty.slave.spawn_command(cmd).expect("spawn euler tui");
        drop(pty.slave);
        let writer = pty.master.take_writer().expect("pty writer");
        let reader = pty.master.try_clone_reader().expect("pty reader");
        let rx = spawn_pty_reader(reader);
        Self {
            child,
            writer,
            rx,
            output: Vec::new(),
            cursor_report_scan_offset: 0,
            master: pty.master,
            resizes: Vec::new(),
        }
    }

    /// Resize the PTY mid-session, recording the output offset so final-state
    /// reconstruction can resize its emulator at the same point.
    fn resize(&mut self, rows: u16, cols: u16) -> usize {
        // Drain pending output so the offset is accurate.
        let deadline = Instant::now() + Duration::from_millis(300);
        let _ = self.wait_for_stable_screen(Duration::from_millis(250), |_| false);
        let _ = deadline;
        let output_offset = self.output.len();
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("pty resize");
        self.resizes.push((output_offset, rows, cols));
        output_offset
    }

    fn write(&mut self, input: &str) {
        self.writer
            .write_all(input.as_bytes())
            .expect("write pty input");
        self.writer.flush().expect("flush pty input");
    }

    fn quit(&mut self) {
        assert!(
            self.wait_ready_composer(),
            "TUI was not ready to quit:\n{}",
            self.screen_text()
        );
        self.write("\x03");
        // Loaded CI runners can take well past the standard 5s window to
        // flush the armed notice through the PTY (observed on shared
        // runners); use a generous ceiling here — this wait is on the exit
        // path, so a fast local run never pays it.
        assert!(
            self.wait_for_stable_screen(Duration::from_secs(20), |screen| {
                screen.contains("ctrl+c again to quit · session saved, /resume restores")
            }),
            "TUI did not arm quit notice:\n{}",
            self.screen_text()
        );
        self.write("\x03");
        assert!(
            self.wait_success(),
            "TUI did not exit cleanly:\n{}",
            self.screen_text()
        );
    }

    fn wait_for_screen(&mut self, needle: &str) -> bool {
        self.wait_for_stable_screen(Duration::from_secs(5), |screen| screen.contains(needle))
    }

    /// Like `wait_for_screen` but without the quiet-interval requirement:
    /// returns as soon as the needle is visible at all. Needed for
    /// transient mid-turn content (e.g. the live streaming reasoning body)
    /// that is on screen while the working HUD spinner repaints every
    /// ~90ms — those repaint chunks reset the 100ms quiet window, so a
    /// stable-screen wait can starve until the transient content is gone.
    fn wait_for_screen_glimpse(&mut self, needle: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.screen_text().contains(needle) {
                return true;
            }
            self.read_next_chunk(deadline);
        }
        self.screen_text().contains(needle)
    }

    fn wait_ready_composer(&mut self) -> bool {
        self.wait_for_stable_screen(Duration::from_secs(5), screen_has_ready_composer)
    }

    /// Wait for a completed turn's persisted event while continuing to drain
    /// the PTY. Rendered streaming text alone is not a completion barrier:
    /// the final delta can arrive before the session commits its assistant
    /// message and before the TUI accepts the next composer submission.
    fn wait_for_home_session_event_count(
        &mut self,
        home: &Path,
        kind: &str,
        expected: usize,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if home_session_persisted_event_count(home, kind).is_some_and(|count| count >= expected)
            {
                return true;
            }
            self.read_next_chunk(deadline);
        }
        home_session_persisted_event_count(home, kind).is_some_and(|count| count >= expected)
    }

    /// Wait for a terminal sequence emitted after a specific output offset.
    /// Resize replays use the scrollback purge as their observable completion
    /// point, so the next submission cannot race the debounce/repaint path.
    fn wait_for_output_after(&mut self, offset: usize, needle: &[u8]) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.output[offset..]
                .windows(needle.len())
                .any(|window| window == needle)
            {
                return true;
            }
            self.read_next_chunk(deadline);
        }
        self.output[offset..]
            .windows(needle.len())
            .any(|window| window == needle)
    }

    fn wait_for_stable_screen(
        &mut self,
        timeout: Duration,
        predicate: impl Fn(&str) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        let mut matched_at_output_len: Option<usize> = None;
        let mut quiet_since: Option<Instant> = None;

        while Instant::now() < deadline {
            let read_chunk = self.read_next_chunk(deadline);
            let screen = self.screen_text();
            if predicate(&screen) {
                if matched_at_output_len != Some(self.output.len()) {
                    matched_at_output_len = Some(self.output.len());
                    quiet_since = Some(Instant::now());
                } else if read_chunk {
                    quiet_since = Some(Instant::now());
                }

                if quiet_since.is_some_and(|since| since.elapsed() >= PTY_QUIET_INTERVAL) {
                    return true;
                }
            } else {
                matched_at_output_len = None;
                quiet_since = None;
            }
        }

        predicate(&self.screen_text())
    }

    fn wait_success(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("poll child") {
                return status.success();
            }
            self.read_next_chunk(deadline);
        }
        false
    }

    fn read_next_chunk(&mut self, deadline: Instant) -> bool {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if let Ok(chunk) = self.rx.recv_timeout(timeout.min(PTY_POLL_INTERVAL)) {
            self.output.extend(chunk);
            self.answer_cursor_report_requests();
            while let Ok(chunk) = self.rx.try_recv() {
                self.output.extend(chunk);
                self.answer_cursor_report_requests();
            }
            true
        } else {
            false
        }
    }

    fn answer_cursor_report_requests(&mut self) {
        let mut search_start = self.cursor_report_scan_offset.saturating_sub(4);
        while let Some((offset, len)) = next_cursor_report_request(&self.output[search_start..]) {
            let request_start = search_start + offset;
            let request_end = request_start + len;
            if request_start < self.cursor_report_scan_offset {
                search_start = request_end;
                continue;
            }
            self.writer
                .write_all(b"\x1b[24;1R")
                .expect("write cursor position report");
            self.writer.flush().expect("flush cursor position report");
            self.cursor_report_scan_offset = request_end;
            search_start = request_end;
        }
        self.cursor_report_scan_offset = self
            .cursor_report_scan_offset
            .max(self.output.len().saturating_sub(4));
    }

    fn screen_text(&self) -> String {
        // Parse at a fixed oversize grid: every app row lands on its own
        // emulator row regardless of mid-session resizes, which is all the
        // contains()-based predicates need. (Feeding vt100 set_size() during
        // a drag trips a subtract-overflow panic inside the crate.)
        let mut parser = vt100::Parser::new(80, 260, 0);
        parser.process(&self.output);
        parser.screen().contents()
    }
}

fn next_cursor_report_request(bytes: &[u8]) -> Option<(usize, usize)> {
    let plain = bytes
        .windows(4)
        .position(|window| window == b"\x1b[6n")
        .map(|offset| (offset, 4));
    let private = bytes
        .windows(5)
        .position(|window| window == b"\x1b[?6n")
        .map(|offset| (offset, 5));
    match (plain, private) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(found), None) | (None, Some(found)) => Some(found),
        (None, None) => None,
    }
}

fn screen_has_ready_composer(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().collect();
    lines.iter().enumerate().any(|(index, line)| {
        if !line.trim_start().starts_with("\u{258c}") {
            return false;
        }
        let Some(next) = lines.get(index + 1) else {
            return false;
        };
        if status_line_marks_ready_composer(next) {
            return true;
        }
        next.trim().is_empty()
            && lines
                .get(index + 2)
                .is_some_and(|after_spacer| status_line_marks_ready_composer(after_spacer))
    })
}

fn status_line_marks_ready_composer(line: &str) -> bool {
    line.contains("ctx ") || line.contains("canvas ")
}

impl Drop for PtyHarness {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
    }
}

fn spawn_pty_reader(mut reader: Box<dyn Read + Send>) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

fn spawn_line_reader(reader: impl Read + Send + 'static) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line.trim_end().to_owned()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

fn run_euler_with_input(exe: &str, args: &[&str], input: &str) -> std::process::Output {
    let home = isolated_home();
    let mut child = command_with_home(exe, &home)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait for euler")
}

fn replay_transcript_with_home(exe: &str, home: &Path, log: &Path) -> String {
    let output = Command::new(exe)
        .env("HOME", home)
        .arg("--replay")
        .arg(log)
        .output()
        .expect("replay");
    assert!(output.status.success());
    String::from_utf8(output.stdout).expect("utf8 transcript")
}

fn only_home_session_id(home: &Path) -> String {
    let ids = home_session_ids(home);
    assert_eq!(ids.len(), 1, "expected one home session in {home:?}");
    ids.into_iter().next().expect("id")
}

fn home_session_ids(home: &Path) -> Vec<String> {
    let sessions = home.join(".euler").join("sessions");
    let index = fs::read_to_string(sessions.join("index.jsonl")).expect("index");
    index
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("index json"))
        .filter_map(|entry| {
            (entry.get("op").and_then(serde_json::Value::as_str) == Some("created"))
                .then(|| entry.get("id")?.as_str().map(str::to_owned))?
        })
        .collect::<Vec<_>>()
}

fn home_index_line_count(home: &Path) -> usize {
    fs::read_to_string(home.join(".euler").join("sessions").join("index.jsonl"))
        .expect("index")
        .lines()
        .count()
}

fn home_index_ops(home: &Path) -> Vec<String> {
    fs::read_to_string(home.join(".euler").join("sessions").join("index.jsonl"))
        .expect("index")
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .expect("index json")
                .get("op")
                .and_then(serde_json::Value::as_str)
                .expect("index op")
                .to_owned()
        })
        .collect()
}

fn home_session_log(home: &Path, session_id: &str) -> std::path::PathBuf {
    home.join(".euler")
        .join("sessions")
        .join(session_id)
        .join("events.jsonl")
}

/// Read whatever complete JSONL records are currently visible. The TUI owns
/// the writer, so this intentionally tolerates a partial final line while it
/// polls for a turn-completion barrier.
fn home_session_persisted_event_count(home: &Path, kind: &str) -> Option<usize> {
    let sessions = home.join(".euler").join("sessions");
    let session_id = fs::read_to_string(sessions.join("index.jsonl"))
        .ok()?
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|entry| {
            (entry.get("op").and_then(serde_json::Value::as_str) == Some("created"))
                .then(|| entry.get("id")?.as_str().map(str::to_owned))?
        })?;
    let events = fs::read_to_string(sessions.join(session_id).join("events.jsonl")).ok()?;
    Some(
        events
            .lines()
            .filter_map(|line| EventEnvelope::from_json_line(line).ok())
            .filter(|event| event.kind.as_str() == kind)
            .count(),
    )
}

fn append_session_rename_event(log: &Path, session_id: &str, name: &str) {
    let parent = read_jsonl(log).last().map(|event| event.id.clone());
    let event = EventEnvelope::new(
        session_id.to_owned(),
        "root",
        parent,
        EventKind::SESSION_RENAMED,
        object([("name", name.into())]),
    );
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(log)
        .expect("open log");
    file.write_all(event.to_json_line().expect("serialize rename").as_bytes())
        .expect("write rename");
    file.write_all(b"\n").expect("finish rename");
}

fn append_unknown_event(log: &Path, session_id: &str) {
    let parent = read_jsonl(log).last().map(|event| event.id.clone());
    let event = EventEnvelope::new(
        session_id.to_owned(),
        "root",
        parent,
        "future.kind",
        object([]),
    );
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(log)
        .expect("open log");
    file.write_all(event.to_json_line().expect("serialize unknown").as_bytes())
        .expect("write unknown");
    file.write_all(b"\n").expect("finish unknown");
}

fn append_missing_blob_event(log: &Path, session_id: &str) {
    let parent = read_jsonl(log).last().map(|event| event.id.clone());
    let mut event = EventEnvelope::new(
        session_id.to_owned(),
        "root",
        parent,
        EventKind::TOOL_RESULT,
        object([
            ("id", "call-read".into()),
            ("name", "read_file".into()),
            ("ok", true.into()),
            ("output", format!("blob:{BLOB_HASH}").into()),
        ]),
    );
    event
        .blobs
        .insert("output".to_owned(), BLOB_HASH.to_owned());
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(log)
        .expect("open log");
    file.write_all(
        event
            .to_json_line()
            .expect("serialize blob event")
            .as_bytes(),
    )
    .expect("write blob event");
    file.write_all(b"\n").expect("finish blob event");
}

fn append_raw_to_log(log: &Path, raw: &[u8]) {
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(log)
        .expect("open log");
    file.write_all(raw).expect("write raw");
}

fn resume_home_session_expect_failure_without_log_change(
    home: &tempfile::TempDir,
    session_id: &str,
) -> std::process::Output {
    let log = home_session_log(home.path(), session_id);
    let index = home
        .path()
        .join(".euler")
        .join("sessions")
        .join("index.jsonl");
    let before_log = fs::read(&log).expect("read log before");
    let before_index = fs::read(&index).expect("read index before");
    let exe = env!("CARGO_BIN_EXE_euler");

    let output = command_with_home(exe, home)
        .arg("--resume")
        .arg(session_id)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("resume invalid");

    assert!(!output.status.success());
    assert_eq!(fs::read(&log).expect("read log after"), before_log);
    assert_eq!(fs::read(&index).expect("read index after"), before_index);
    output
}

fn replay_transcript(exe: &str, log: &Path) -> String {
    let home = isolated_home();
    let output = command_with_home(exe, &home)
        .arg("--replay")
        .arg(log)
        .output()
        .expect("replay");
    assert!(output.status.success());
    String::from_utf8(output.stdout).expect("utf8 transcript")
}

fn isolated_home() -> tempfile::TempDir {
    tempfile::tempdir().expect("isolated HOME")
}

fn record_recent_catalog_refresh_for_test(home: &Path) {
    let directory = home.join(".euler").join("catalogs").join("provider-v1");
    fs::create_dir_all(&directory).expect("catalog state directory");
    let attempted_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    fs::write(
        directory.join(".refresh-state-v1.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1,
            "attempted_at": attempted_at,
            "outcome": "succeeded",
            "release_id": null,
        }))
        .expect("catalog state JSON"),
    )
    .expect("catalog state");
}

fn command_with_home(exe: &str, home: &tempfile::TempDir) -> Command {
    let mut command = Command::new(exe);
    command.env("HOME", home.path());
    // Pin the working directory to the isolated home. Sessions record their
    // workspace and resume only from it (ADR 0017 same-workspace resume),
    // and portable-pty defaults a spawned TUI's cwd to $HOME — one shared
    // default keeps create/resume pairs in the same workspace and keeps
    // project-context preflight away from the real checkout. Tests that
    // need a specific workspace override this with `.current_dir(...)`.
    command.current_dir(home.path());
    command
}

fn configure_linked_extension(exe: &str, home: &tempfile::TempDir, path: &Path, id: &str) {
    for args in [
        vec!["extension", "link", path_str(path)],
        vec!["extension", "enable", id],
    ] {
        let output = command_with_home(exe, home)
            .args(args)
            .output()
            .expect("configure linked extension");
        assert!(
            output.status.success(),
            "configuration stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn command_with_secret_env(
    exe: &str,
    home: &tempfile::TempDir,
    secrets: &SecretFixture,
) -> Command {
    let mut command = command_with_home(exe, home);
    command
        // Keep the fixture path deterministic even when the parent shell has
        // Euler runtime selection variables set.
        .env_remove("EULER_PROVIDER")
        .env_remove("EULER_MODEL")
        .env_remove("EULER_HOME")
        .env("ANTHROPIC_API_KEY", ANTHROPIC_API_KEY_SENTINEL)
        .env("OPENAI_API_KEY", OPENAI_API_KEY_SENTINEL)
        .env("OPENROUTER_API_KEY", OPENROUTER_API_KEY_SENTINEL)
        .env("XAI_API_KEY", XAI_API_KEY_SENTINEL)
        .env("EULER_AUTH_FILE", &secrets.auth_file)
        .env("EULER_CUSTOM_API_KEY", EULER_CUSTOM_API_KEY_SENTINEL)
        .env("AWS_SECRET_ACCESS_KEY", AWS_SECRET_ACCESS_KEY_SENTINEL)
        .env("EULER_TEST_TOKEN", EULER_TEST_TOKEN_SENTINEL)
        .env("EULER_TEST_SECRET", EULER_TEST_SECRET_SENTINEL);
    command
}

fn assert_fixture_input_is_secret_free(input: &str, sentinels: &[SecretSentinel]) {
    let bytes = input.as_bytes();
    for sentinel in sentinels {
        assert!(
            !contains_bytes(bytes, &sentinel.value),
            "fixture input contains {} sentinel",
            sentinel.label
        );
    }
}

fn collect_home_session_artifacts(home: &Path, known_event_text: &str) -> Vec<std::path::PathBuf> {
    let sessions = home.join(".euler").join("sessions");
    let index = sessions.join("index.jsonl");
    assert_nonempty_file(&index);
    let index_text = fs::read_to_string(&index).expect("read index");
    assert!(index_text.contains(r#""op":"created""#));

    let session_dirs = session_dirs(&sessions);
    assert_eq!(session_dirs.len(), 1, "expected one home session");

    let mut artifacts = vec![index];
    let mut event_text = String::new();
    for session_dir in session_dirs {
        let session_json = session_dir.join("session.json");
        let events_jsonl = session_dir.join("events.jsonl");
        let blobs = session_dir.join("blobs");
        assert_nonempty_file(&session_json);
        assert_nonempty_file(&events_jsonl);
        assert!(blobs.is_dir(), "expected blob directory at {blobs:?}");
        let metadata = fs::read_to_string(&session_json).expect("read metadata");
        assert!(metadata.contains(r#""events_path""#));
        assert!(metadata.contains(r#""blobs_dir""#));

        event_text.push_str(&fs::read_to_string(&events_jsonl).expect("read events"));
        artifacts.push(session_json);
        artifacts.push(events_jsonl);
        // The line-oriented echo fixture does not force blob production today.
        // If any blob files are produced, this discovery path scans them.
        artifacts.extend(blob_files(&blobs));
    }
    assert!(event_text.contains(known_event_text));
    artifacts
}

fn session_dirs(sessions: &Path) -> Vec<std::path::PathBuf> {
    let mut dirs = fs::read_dir(sessions)
        .expect("read sessions dir")
        .map(|entry| entry.expect("session entry").path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    dirs.sort();
    dirs
}

fn blob_files(blobs: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![blobs.to_owned()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read blob dir") {
            let path = entry.expect("blob entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn assert_nonempty_file(path: &Path) {
    let metadata = fs::metadata(path).unwrap_or_else(|error| {
        panic!("expected file at {path:?}: {error}");
    });
    assert!(metadata.is_file(), "expected file at {path:?}");
    assert!(metadata.len() > 0, "expected non-empty file at {path:?}");
}

fn assert_no_sentinels_in_file(path: &Path, sentinels: &[SecretSentinel]) {
    let bytes = fs::read(path).expect("read persisted artifact");
    for sentinel in sentinels {
        assert!(
            !contains_bytes(&bytes, &sentinel.value),
            "{} sentinel leaked into {path:?}",
            sentinel.label
        );
    }
}

fn assert_no_path_leak(text: &str, paths: &[&Path]) {
    for path in paths {
        assert!(
            !text.contains(path.to_string_lossy().as_ref()),
            "path leaked into output: {}",
            path.display()
        );
    }
}

fn audit_issue_code<'a>(audit: &'a serde_json::Value, id: &str) -> &'a str {
    audit["entries"]
        .as_array()
        .expect("audit entries")
        .iter()
        .find(|entry| entry["id"] == id)
        .unwrap_or_else(|| panic!("missing audit entry for {id}"))["issue_code"]
        .as_str()
        .expect("audit issue code")
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn resume_expect_failure_without_log_change(log: &Path) -> std::process::Output {
    let before = fs::read(log).expect("read before");
    let exe = env!("CARGO_BIN_EXE_euler");
    let output = run_euler_with_input(exe, &["--resume", path_str(log)], "");
    assert!(!output.status.success());
    assert_eq!(fs::read(log).expect("read after"), before);
    output
}

fn write_events(path: &Path, events: &[EventEnvelope]) {
    let content = events
        .iter()
        .map(|event| event.to_json_line().expect("serialize event"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(path, format!("{content}\n")).expect("write events");
}

fn write_fixture_script(dir: &Path, name: &str, content: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, content).expect("write fixture script");
    path
}

fn read_diagnostics_jsonl(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .expect("read diagnostics jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).expect("diagnostics line json"))
        .collect()
}

fn assert_schema(line: &serde_json::Value) {
    let object = line.as_object().expect("diagnostics object");
    for key in ["ts", "level", "target", "session_id", "event"] {
        assert!(object.get(key).is_some(), "missing diagnostics key {key}");
        assert!(
            object[key].is_string(),
            "diagnostics key {key} must be string"
        );
    }
    assert_eq!(object["target"], "euler_core::diagnostics");
    assert!(object["event"].as_str().expect("event").contains('_'));
}

fn has_diagnostic_event(lines: &[serde_json::Value], event: &str) -> bool {
    lines
        .iter()
        .any(|line| line.get("event").and_then(serde_json::Value::as_str) == Some(event))
}

fn write_blob_reference_event(log: &Path) {
    let mut event = EventEnvelope::new(
        "s",
        "a",
        None,
        EventKind::TOOL_RESULT,
        object([
            ("id", "call-read".into()),
            ("name", "read_file".into()),
            ("ok", true.into()),
            ("output", format!("blob:{BLOB_HASH}").into()),
        ]),
    );
    event
        .blobs
        .insert("output".to_owned(), BLOB_HASH.to_owned());
    write_events(log, &[event]);
}

fn read_jsonl(path: &Path) -> Vec<EventEnvelope> {
    fs::read_to_string(path)
        .expect("read jsonl")
        .lines()
        .map(|line| EventEnvelope::from_json_line(line).expect("json event"))
        .collect()
}

fn write_registry_state(home: &tempfile::TempDir, id: &str, op: &str) {
    let extensions_dir = home.path().join(".euler").join("extensions");
    fs::create_dir_all(&extensions_dir).expect("registry dir");
    fs::write(
        extensions_dir.join("state.jsonl"),
        format!(
            r#"{{"v":1,"op":"{op}","id":"{id}","ts_ms":1}}
"#
        ),
    )
    .expect("registry state");
}

fn assert_no_forbidden_text(label: &str, text: &str, forbidden: &[&str]) {
    for value in forbidden {
        if value.is_empty() {
            continue;
        }
        assert!(
            !text.contains(value),
            "{label} leaked forbidden string `{value}`"
        );
    }
}

fn recovery_closure_count(events: &[EventEnvelope]) -> usize {
    events
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::TOOL_RESULT
                && event
                    .payload
                    .get("recovery_closure")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
        })
        .count()
}

fn tool_call(id: &str, name: &str) -> EventEnvelope {
    EventEnvelope::new(
        "s",
        "a",
        None,
        EventKind::TOOL_CALL,
        object([
            ("id", id.to_owned().into()),
            ("name", name.to_owned().into()),
            ("input", serde_json::json!({})),
        ]),
    )
}

fn session_start(provider: &str, model: &str) -> EventEnvelope {
    EventEnvelope::new(
        "s",
        "a",
        None,
        EventKind::SESSION_START,
        object([
            ("provider", provider.to_owned().into()),
            ("model", model.to_owned().into()),
        ]),
    )
}

fn lock_path_for(log: &Path) -> std::path::PathBuf {
    log.with_file_name(format!(
        "{}.lock",
        log.file_name().expect("log filename").to_string_lossy()
    ))
}

/// The session writer records its `LockOwnerMetadata` (a JSON object carrying
/// `pid`) into the lock file *after* the OS advisory lock is granted, so a
/// readable owner record proves the flock is held — not merely that the lock
/// file was created. Returns the owning PID once that record is present.
fn lock_owner_pid(lock: &Path) -> Option<u32> {
    let bytes = std::fs::read(lock).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("pid")?.as_u64().map(|pid| pid as u32)
}

/// Block until `first` provably *holds* the session advisory lock, then return.
///
/// Synchronizes on an observed state transition (the owner record the writer
/// persists once the flock is granted), not on a wall-clock startup deadline.
/// The old tests polled `lock.exists()` under a fixed 1s window, which raced
/// first-process startup under load (issue #145: "first euler did not create
/// its lock") and also admitted the window between lock-file creation and flock
/// acquisition. Waiting for the owner record closes both gaps. The record must
/// carry `first`'s own PID: core writes the metadata best-effort and calls it
/// non-authoritative, so equality is what ties the observed record to the
/// process this test spawned rather than to any stale or foreign writer. The
/// 60s liveness deadline is a hang detector, not a duration proxy — healthy
/// startup is under a second even on a loaded runner — and it keeps a
/// regression a clear failure instead of a stuck job.
fn wait_until_session_lock_held(lock: &Path, first: &mut std::process::Child) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let observed = lock_owner_pid(lock);
        if observed == Some(first.id()) {
            return;
        }
        if let Some(status) = first.try_wait().expect("poll first euler for lock") {
            panic!("first euler exited before acquiring its session lock: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "first euler (pid {}) did not hold its session lock within 60s; \
             observed owner record: {observed:?}",
            first.id()
        );
        // Polite poll: a sleep genuinely deschedules (yield_now need not, per
        // the drain-helper rationale) so this loop cannot compete with the
        // very process startup it is waiting on.
        thread::sleep(Duration::from_millis(5));
    }
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn write_extension_manifest(dir: &Path, id: &str, version: &str) {
    fs::write(
        dir.join(euler_core::EXTENSION_MANIFEST_FILE),
        format!(
            r#"{{
  "version": 1,
  "id": "{id}",
  "display_name": "Example Extension",
  "extension_version": "{version}",
  "runtime_kind": "native-rust",
  "capabilities": ["provenance-read"],
  "commands": [
    {{
      "name": "inspect",
      "display_name": "Inspect",
      "summary": "Inspect provenance.",
      "required_capabilities": ["provenance-read"]
    }}
  ]
}}"#
        ),
    )
    .expect("write extension manifest");
}

fn write_managed_process_extension_manifest(
    dir: &Path,
    id: &str,
    version: &str,
    command: &[String],
) {
    let manifest = serde_json::json!({
        "version": 1,
        "id": id,
        "display_name": "Python CLI proof",
        "extension_version": version,
        "runtime_kind": "managed-process",
        "entrypoint": {"command": command},
        "capabilities": ["provenance-read", "artifact-write"],
        "commands": [{
            "name": "inspect",
            "display_name": "Inspect",
            "summary": "Read provenance and write an artifact.",
            "required_capabilities": ["provenance-read", "artifact-write"]
        }]
    });
    fs::write(
        dir.join(euler_core::EXTENSION_MANIFEST_FILE),
        serde_json::to_vec_pretty(&manifest).expect("serialize managed-process manifest"),
    )
    .expect("write managed-process extension manifest");
}

#[cfg(unix)]
#[test]
fn headless_extension_run_executes_enabled_linked_python_process_live() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let extension_dir = tempfile::tempdir().expect("extension dir");
    let python = PathBuf::from("python3");
    write_managed_process_extension_manifest(
        extension_dir.path(),
        "python-live-proof",
        "0.1.1",
        &[
            python.to_string_lossy().into_owned(),
            "-B".to_owned(),
            "-u".to_owned(),
            "extension.py".to_owned(),
        ],
    );
    let sdk_source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../python/euler_managed_process_sdk/src");
    fs::write(
        extension_dir.path().join("extension.py"),
        format!(
            r#"import sys
sys.path.insert(0, {sdk_source:?})
from euler_managed_process_sdk import serve

def inspect(context):
    page = context.host.query_provenance(limit=16, scan_limit=32)
    return {{"tag": context.input["tag"], "seen_events": len(page["events"])}}

serve({{"inspect": inspect}})
"#,
            sdk_source = sdk_source.to_string_lossy()
        ),
    )
    .expect("write Python extension");

    for args in [
        vec!["extension", "link", path_str(extension_dir.path())],
        vec!["extension", "enable", "python-live-proof"],
    ] {
        let output = command_with_home(exe, &home)
            .args(args)
            .output()
            .expect("configure linked extension");
        assert!(
            output.status.success(),
            "configuration stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let root = tempfile::tempdir().expect("root dir");
    let log = root.path().join("events.jsonl");
    let mut child = command_with_home(exe, &home)
        .current_dir(root.path())
        .arg("--provenance")
        .arg(path_str(&log))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(
            b"seed live process\nextension_run python-live-proof.inspect {\"tag\":\"live\"}\n",
        )
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait euler");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = String::from_utf8(output.stdout)
        .expect("stdout utf8")
        .lines()
        .find(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("result json"))
        .expect("extension result");
    assert_eq!(result["type"], serde_json::json!("extension_run_result"));
    assert_eq!(result["extension"], serde_json::json!("python-live-proof"));
    assert_eq!(result["result"]["tag"], serde_json::json!("live"));
    assert!(result["result"]["seen_events"].as_u64().unwrap() >= 1);
}

#[cfg(unix)]
fn provision_python_venv(extension_dir: &Path) -> PathBuf {
    let sdk_source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../python/euler_managed_process_sdk");
    let sdk_copy = extension_dir.join("sdk-package");
    copy_directory(&sdk_source, &sdk_copy);
    let venv = extension_dir.join(".venv");
    let created = Command::new("python3")
        .args(["-m", "venv"])
        .arg(&venv)
        .status()
        .expect("create Python virtual environment");
    assert!(created.success(), "python3 -m venv failed: {created}");
    let python = venv.join("bin/python");
    // Editable-install equivalent without pip or a build backend (issue
    // #142): the SDK is pure Python, so path-linking its src/ through a
    // .pth file in the venv's site-packages is everything `pip install -e`
    // would achieve here — while staying offline (CI requirement) and
    // independent of whether the host Python still bundles setuptools.
    // Python 3.12+ venvs do not, and modern system interpreters (e.g.
    // Homebrew 3.14) ship none, which made the previous
    // `--no-build-isolation` editable install fail with
    // `Cannot import 'setuptools.build_meta'`.
    let purelib = Command::new(&python)
        .args([
            "-c",
            "import sysconfig; print(sysconfig.get_paths()['purelib'])",
        ])
        .output()
        .expect("resolve venv site-packages");
    assert!(
        purelib.status.success(),
        "resolving venv site-packages failed: {}",
        String::from_utf8_lossy(&purelib.stderr)
    );
    let site_packages = PathBuf::from(String::from_utf8_lossy(&purelib.stdout).trim());
    fs::write(
        site_packages.join("euler_managed_process_sdk.pth"),
        format!("{}\n", sdk_copy.join("src").display()),
    )
    .expect("write SDK path link");
    let imports = Command::new(&python)
        .args(["-B", "-c", "import euler_managed_process_sdk"])
        .output()
        .expect("verify SDK import");
    assert!(
        imports.status.success(),
        "venv python cannot import the SDK: {}",
        String::from_utf8_lossy(&imports.stderr)
    );
    python
}

fn copy_directory(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("create copied SDK directory");
    for entry in fs::read_dir(source).expect("read SDK directory") {
        let entry = entry.expect("SDK entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("SDK entry type").is_dir() {
            copy_directory(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy SDK file");
        }
    }
}

/// Like [`run_euler_with_input`], but from an explicit working directory —
/// resumes must run in the workspace the session was recorded in (ADR 0017
/// same-workspace resume).
fn run_euler_with_input_from(
    exe: &str,
    args: &[&str],
    input: &str,
    cwd: &Path,
) -> std::process::Output {
    let home = isolated_home();
    let mut child = command_with_home(exe, &home)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait for euler")
}

/// The acknowledgment card (ADR 0017 phase 3) is presented before the session
/// starts when a fresh interactive `auto` session finds unacknowledged
/// EULER.md guidance. A single keypress accepts it and writes a durable
/// acknowledgment under the user home.
#[test]
fn tui_pty_acknowledgment_card_accept_loads_and_records() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
    std::fs::write(workspace.join("EULER.md"), "Prefer small commits.").expect("euler md");

    let script = write_fixture_script(
        temp.path(),
        "ack-card-accept.json",
        &serde_json::json!({
            "version": 1,
            "responses": [{"events": [
                {"text_delta": "hello"},
                {"finished": {"stop_reason": "completed"}}
            ]}]
        })
        .to_string(),
    );
    let script_option = format!("event-script={}", path_str(&script));
    let mut tui = PtyHarness::spawn_with_args_in_dir(
        &home,
        Some(&workspace),
        &[
            "tui",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
        ],
    );

    assert!(
        tui.wait_for_screen("Load this project's guidance?"),
        "acknowledgment card did not render:\n{}",
        tui.screen_text()
    );
    assert!(
        tui.screen_text().contains("EULER.md"),
        "the card must list the discovered file:\n{}",
        tui.screen_text()
    );
    tui.write("y");
    assert!(
        tui.wait_for_screen("/ commands"),
        "the app did not start after accepting:\n{}",
        tui.screen_text()
    );
    tui.quit();

    let ack_dir = home.join(".euler").join("project-context");
    let recorded = std::fs::read_dir(&ack_dir)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(
        recorded, 1,
        "accepting must write one durable acknowledgment"
    );
}

/// Skipping the acknowledgment card starts the session without the guidance
/// and writes no durable record (decline is session-only).
#[test]
fn tui_pty_acknowledgment_card_skip_writes_no_record() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
    std::fs::write(workspace.join("EULER.md"), "Prefer small commits.").expect("euler md");

    let script = write_fixture_script(
        temp.path(),
        "ack-card-skip.json",
        &serde_json::json!({
            "version": 1,
            "responses": [{"events": [
                {"text_delta": "hello"},
                {"finished": {"stop_reason": "completed"}}
            ]}]
        })
        .to_string(),
    );
    let script_option = format!("event-script={}", path_str(&script));
    let mut tui = PtyHarness::spawn_with_args_in_dir(
        &home,
        Some(&workspace),
        &[
            "tui",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
        ],
    );

    assert!(
        tui.wait_for_screen("Load this project's guidance?"),
        "acknowledgment card did not render:\n{}",
        tui.screen_text()
    );
    tui.write("n");
    assert!(
        tui.wait_for_screen("/ commands"),
        "the app did not start after skipping:\n{}",
        tui.screen_text()
    );
    tui.quit();

    let ack_dir = home.join(".euler").join("project-context");
    let recorded = std::fs::read_dir(&ack_dir)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(
        recorded, 0,
        "skipping must not write a durable acknowledgment"
    );
}

/// Headless resume into a different folder fails closed with a plain-language
/// remediation, and `--accept-relocation` is the scripted yes that moves the
/// session and records a durable `project.context.relocated` event
/// (ADR 0017 phase 3).
#[test]
fn exec_resume_relocation_requires_accept_relocation_flag() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let home = isolated_home();
    let temp = tempfile::tempdir().expect("temp");
    let workspace_a = temp.path().join("a");
    let workspace_b = temp.path().join("b");
    std::fs::create_dir_all(&workspace_a).expect("a");
    std::fs::create_dir_all(&workspace_b).expect("b");
    let log = temp.path().join("session.jsonl");
    let script = write_fixture_script(
        temp.path(),
        "reloc.json",
        &serde_json::json!({
            "version": 1,
            "responses": [{"events": [
                {"text_delta": "ok"},
                {"finished": {"stop_reason": "completed"}}
            ]}]
        })
        .to_string(),
    );
    let script_option = format!("event-script={}", path_str(&script));

    // Create the session in workspace A.
    let create = command_with_home(exe, &home)
        .current_dir(&workspace_a)
        .args([
            "exec",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
            "--provenance",
            path_str(&log),
            "--auto-approve",
            "read-only",
            "first prompt",
        ])
        .output()
        .expect("create session");
    assert!(
        create.status.success(),
        "create: {}",
        String::from_utf8_lossy(&create.stderr)
    );

    // Resume from workspace B WITHOUT the flag: fails closed with remediation.
    let denied = command_with_home(exe, &home)
        .current_dir(&workspace_b)
        .args([
            "exec",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
            "--resume",
            path_str(&log),
            "--auto-approve",
            "read-only",
            "second prompt",
        ])
        .output()
        .expect("denied resume");
    assert!(
        !denied.status.success(),
        "resume from a new folder must fail without the flag"
    );
    let stderr = String::from_utf8_lossy(&denied.stderr);
    assert!(
        stderr.contains("--accept-relocation"),
        "remediation must mention the flag:\n{stderr}"
    );
    // Nothing was appended.
    assert!(!read_jsonl(&log)
        .iter()
        .any(|event| event.kind.as_str() == EventKind::PROJECT_CONTEXT_RELOCATED));

    // Resume from workspace B WITH the flag: succeeds and records the event.
    let moved = command_with_home(exe, &home)
        .current_dir(&workspace_b)
        .args([
            "exec",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
            "--resume",
            path_str(&log),
            "--accept-relocation",
            "--auto-approve",
            "read-only",
            "second prompt",
        ])
        .output()
        .expect("moved resume");
    assert!(
        moved.status.success(),
        "accepted relocation: {}",
        String::from_utf8_lossy(&moved.stderr)
    );
    let events = read_jsonl(&log);
    assert!(
        events
            .iter()
            .any(|event| event.kind.as_str() == EventKind::PROJECT_CONTEXT_RELOCATED),
        "an accepted relocation must record a durable project.context.relocated event"
    );
}

/// The relocation-consent card (ADR 0017 phase 3) is presented before a
/// resumed session is rebuilt when the live folder differs from where the
/// session last ran. Accepting moves the session and records the event.
#[test]
fn tui_pty_relocation_card_accept_resumes_and_records() {
    let exe = env!("CARGO_BIN_EXE_euler");
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("home");
    let workspace_a = temp.path().join("a");
    let workspace_b = temp.path().join("b");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&workspace_a).expect("a");
    std::fs::create_dir_all(&workspace_b).expect("b");
    let log = temp.path().join("session.jsonl");
    let script = write_fixture_script(
        temp.path(),
        "reloc-tui.json",
        &serde_json::json!({
            "version": 1,
            "responses": [{"events": [
                {"text_delta": "ok"},
                {"finished": {"stop_reason": "completed"}}
            ]}]
        })
        .to_string(),
    );
    let script_option = format!("event-script={}", path_str(&script));

    // Create the session in workspace A.
    let create = std::process::Command::new(exe)
        .env("HOME", &home)
        .current_dir(&workspace_a)
        .args([
            "exec",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
            "--provenance",
            path_str(&log),
            "--auto-approve",
            "read-only",
            "first prompt",
        ])
        .output()
        .expect("create session");
    assert!(
        create.status.success(),
        "create: {}",
        String::from_utf8_lossy(&create.stderr)
    );

    // Resume via TUI from workspace B: the relocation card appears first.
    let mut tui = PtyHarness::spawn_with_args_in_dir(
        &home,
        Some(&workspace_b),
        &["--resume", path_str(&log)],
    );
    assert!(
        tui.wait_for_screen("This session last ran in a different folder"),
        "relocation card did not render:\n{}",
        tui.screen_text()
    );
    tui.write("r");
    assert!(
        tui.wait_for_screen("/ commands"),
        "resumed app did not start after accepting relocation:\n{}",
        tui.screen_text()
    );
    tui.quit();

    let events = read_jsonl(&log);
    assert!(
        events
            .iter()
            .any(|event| event.kind.as_str() == EventKind::PROJECT_CONTEXT_RELOCATED),
        "accepting the card must record a durable project.context.relocated event"
    );
}

/// An in-app `/new` after EULER.md appears (or changes) must present the
/// acknowledgment card before composing the fresh session (ADR 0017 decision
/// 13, blocker 4). Accepting loads it and records a durable acknowledgment.
#[test]
fn tui_pty_new_session_acknowledgment_card_accept_records() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
    // No EULER.md at launch, so there is no launch card.
    let script = write_fixture_script(
        temp.path(),
        "new-card-accept.json",
        &serde_json::json!({
            "version": 1,
            "responses": [{"events": [
                {"text_delta": "ok"},
                {"finished": {"stop_reason": "completed"}}
            ]}]
        })
        .to_string(),
    );
    let script_option = format!("event-script={}", path_str(&script));
    let mut tui = PtyHarness::spawn_with_args_in_dir(
        &home,
        Some(&workspace),
        &[
            "tui",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
        ],
    );
    assert!(tui.wait_for_screen("/ commands"), "{}", tui.screen_text());
    // The project gains guidance while the session runs.
    std::fs::write(workspace.join("EULER.md"), "Prefer small commits.").expect("euler md");
    tui.write("/new\r");
    assert!(
        tui.wait_for_screen("Load this project's guidance?"),
        "/new did not present the acknowledgment card:\n{}",
        tui.screen_text()
    );
    tui.write("y");
    assert!(
        tui.wait_for_screen("new session"),
        "new session did not start after accepting:\n{}",
        tui.screen_text()
    );
    tui.quit();

    let ack_dir = home.join(".euler").join("project-context");
    let recorded = std::fs::read_dir(&ack_dir)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(
        recorded, 1,
        "accepting the /new card must record one acknowledgment"
    );
}

/// Skipping the `/new` acknowledgment card starts the fresh session without the
/// guidance and writes no durable record (decline is session-only).
#[test]
fn tui_pty_new_session_acknowledgment_card_skip_writes_no_record() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
    let script = write_fixture_script(
        temp.path(),
        "new-card-skip.json",
        &serde_json::json!({
            "version": 1,
            "responses": [{"events": [
                {"text_delta": "ok"},
                {"finished": {"stop_reason": "completed"}}
            ]}]
        })
        .to_string(),
    );
    let script_option = format!("event-script={}", path_str(&script));
    let mut tui = PtyHarness::spawn_with_args_in_dir(
        &home,
        Some(&workspace),
        &[
            "tui",
            "--provider",
            "fixture",
            "--provider-option",
            &script_option,
        ],
    );
    assert!(tui.wait_for_screen("/ commands"), "{}", tui.screen_text());
    std::fs::write(workspace.join("EULER.md"), "Prefer small commits.").expect("euler md");
    tui.write("/new\r");
    assert!(
        tui.wait_for_screen("Load this project's guidance?"),
        "/new did not present the acknowledgment card:\n{}",
        tui.screen_text()
    );
    tui.write("n");
    assert!(
        tui.wait_for_screen("new session"),
        "new session did not start after skipping:\n{}",
        tui.screen_text()
    );
    tui.quit();

    let ack_dir = home.join(".euler").join("project-context");
    let recorded = std::fs::read_dir(&ack_dir)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(
        recorded, 0,
        "skipping the /new card must not record an acknowledgment"
    );
}
