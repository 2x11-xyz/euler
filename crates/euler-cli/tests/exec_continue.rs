//! `euler exec --resume <log> --continue [--retry]` (ADR 0023 rollout step
//! 3). Each interrupted log is a real exec log cut at a line boundary, which
//! is the accepted prefix a crash leaves behind.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const READ_THEN_ANSWER: &str = r#"{
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
        { "text_delta": "first answer" },
        { "finished": { "stop_reason": "completed" } }
      ]
    }
  ]
}
"#;

struct Workspace {
    _home: tempfile::TempDir,
    root: PathBuf,
    log: PathBuf,
    script: PathBuf,
}

/// A workspace that is its own repository, with a fixture script that reads
/// `note.txt` and then answers.
fn workspace() -> Workspace {
    let home = tempfile::tempdir().expect("home dir");
    fs::create_dir(home.path().join(".git")).expect("repository marker");
    let root = home.path().to_path_buf();
    fs::write(root.join("note.txt"), "alpha\n").expect("write note");
    let script = root.join("read-then-answer.json");
    fs::write(&script, READ_THEN_ANSWER).expect("write script");
    Workspace {
        log: root.join("events.jsonl"),
        root,
        script,
        _home: home,
    }
}

fn euler(workspace: &Workspace, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_euler"))
        .env("HOME", &workspace.root)
        .env_remove("EULER_HOME")
        .env_remove("EULER_PROVIDER")
        .env_remove("EULER_MODEL")
        .current_dir(&workspace.root)
        .arg("exec")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait euler")
}

/// Run `--continue` with stdin held open and never written: a read would
/// block, so the run finishing proves stdin was never read.
fn continue_with_open_stdin(workspace: &Workspace) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_euler"))
        .env("HOME", &workspace.root)
        .env_remove("EULER_HOME")
        .env_remove("EULER_PROVIDER")
        .env_remove("EULER_MODEL")
        .current_dir(&workspace.root)
        .args(["exec", "--resume", log_arg(workspace), "--continue"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    let stdin = child.stdin.take().expect("stdin");
    // Drain output while polling so a full pipe cannot pass for a stdin read.
    let stdout = drain(child.stdout.take().expect("stdout"));
    let stderr = drain(child.stderr.take().expect("stderr"));
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll euler") {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().expect("kill euler");
            panic!("euler exec --continue blocked on stdin");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(stdin);
    Output {
        status,
        stdout: stdout.join().expect("stdout reader"),
        stderr: stderr.join().expect("stderr reader"),
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes).expect("read euler output");
        bytes
    })
}

fn log_arg(workspace: &Workspace) -> &str {
    workspace.log.to_str().expect("utf8 log path")
}

fn continue_run(workspace: &Workspace, extra: &[&str], stdin: &str) -> Output {
    let mut args = vec!["--resume", log_arg(workspace), "--continue"];
    args.extend_from_slice(extra);
    euler(workspace, &args, stdin)
}

/// Keep the log through its last event of `kind`.
fn cut_after_last(log: &Path, kind: &str) {
    let content = fs::read_to_string(log).expect("read log");
    let lines = content.lines().collect::<Vec<_>>();
    let keep = lines
        .iter()
        .rposition(|line| kind_of(line) == kind)
        .expect("event kind present");
    fs::write(log, format!("{}\n", lines[..=keep].join("\n"))).expect("cut log");
}

fn kind_of(line: &str) -> String {
    serde_json::from_str::<serde_json::Value>(line).expect("json line")["kind"]
        .as_str()
        .expect("kind")
        .to_owned()
}

fn kinds(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .expect("read log")
        .lines()
        .map(kind_of)
        .collect()
}

/// A finished exec turn cut while its `read_file` call ran. Resume rebuilds
/// the provider from the log without provider options, so continuations run
/// on the fixture echo model, which answers without requesting tools.
fn crashed_during_tool(workspace: &Workspace) {
    let script = format!("event-script={}", workspace.script.display());
    let output = euler(
        workspace,
        &[
            "--provider",
            "fixture",
            "--provider-option",
            &script,
            "--provenance",
            log_arg(workspace),
            "read the note",
        ],
        "",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    cut_after_last(&workspace.log, "tool.call");
}

#[test]
fn continue_finishes_the_interrupted_run_without_reading_stdin() {
    let workspace = workspace();
    crashed_during_tool(&workspace);

    let output = continue_with_open_stdin(&workspace);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("Continuing run"), "{stderr}");
    assert!(stderr.contains("(restart 1 of 5)"), "{stderr}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("assistant: user: read the note"),
        "{stdout}"
    );
    let kinds = kinds(&workspace.log);
    assert_eq!(
        kinds.iter().filter(|kind| *kind == "run.started").count(),
        1
    );
    assert_eq!(
        kinds.iter().filter(|kind| *kind == "user.message").count(),
        1
    );
    assert_eq!(
        kinds.iter().filter(|kind| *kind == "run.continued").count(),
        1
    );
    assert_eq!(
        kinds.iter().filter(|kind| *kind == "run.terminal").count(),
        1
    );

    // Nothing is left to continue; the refusal appends nothing and says how
    // to move on.
    let before = fs::read(&workspace.log).expect("read log");
    let output = continue_run(&workspace, &[], "");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no interrupted run to continue"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("euler exec --resume {}", log_arg(&workspace))),
        "{stderr}"
    );
    assert_eq!(fs::read(&workspace.log).expect("read log"), before);
}

#[test]
fn a_parked_run_explains_why_and_prints_the_retry_command() {
    let workspace = workspace();
    crashed_during_tool(&workspace);
    // Each continuation crashes before its first model call: cut the log
    // right after the `run.continued` it recorded.
    for _ in 0..5 {
        let output = continue_run(&workspace, &[], "");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        cut_after_last(&workspace.log, "run.continued");
    }

    let before = fs::read(&workspace.log).expect("read log");
    let output = continue_run(&workspace, &[], "");
    assert!(!output.status.success());
    assert_eq!(fs::read(&workspace.log).expect("read log"), before);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("is parked (restart_limit)"), "{stderr}");
    assert!(
        stderr.contains("continued 5 times without progress"),
        "{stderr}"
    );
    let retry = format!(
        "euler exec --resume {} --continue --retry",
        log_arg(&workspace)
    );
    assert!(stderr.contains(&retry), "{stderr}");

    let output = continue_run(&workspace, &["--retry"], "");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("(restart 1 of 5)"), "{stderr}");
}
