//! Execution profile selection, validation, and recording through the CLI
//! (ADR 0026 rollout steps 3 to 7: profile plumbing, the `locked`, `full`,
//! and `workspace` presets, escalation, and the headless default).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use sha2::{Digest, Sha256};

struct Workspace {
    _temp: tempfile::TempDir,
    home: PathBuf,
    root: PathBuf,
    log: PathBuf,
}

/// A user home and a separate project that is its own repository.
fn workspace() -> Workspace {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path().join("home");
    let root = temp.path().join("project");
    fs::create_dir_all(home.join(".euler")).expect("euler home");
    fs::create_dir_all(root.join(".git")).expect("repository marker");
    Workspace {
        log: root.join("events.jsonl"),
        home,
        root,
        _temp: temp,
    }
}

fn write_profiles(workspace: &Workspace, text: &str) -> PathBuf {
    let path = workspace.home.join(".euler").join("profiles.json");
    fs::write(&path, text).expect("write profiles.json");
    path
}

fn write_overlay(workspace: &Workspace, text: &str) {
    fs::create_dir_all(workspace.root.join(".euler")).expect("overlay dir");
    fs::write(
        workspace.root.join(".euler").join("profile-overlay.json"),
        text,
    )
    .expect("write overlay");
}

fn exec(workspace: &Workspace, args: &[&str]) -> Output {
    exec_in(workspace, &workspace.root, args)
}

fn exec_in(workspace: &Workspace, root: &Path, args: &[&str]) -> Output {
    exec_with_env(workspace, root, &[], args)
}

fn exec_with_env(
    workspace: &Workspace,
    root: &Path,
    env: &[(&str, &str)],
    args: &[&str],
) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_euler"))
        .envs(env.iter().copied())
        .env("HOME", &workspace.home)
        .env_remove("EULER_HOME")
        .env_remove("EULER_PROVIDER")
        .env_remove("EULER_MODEL")
        .current_dir(root)
        .arg("exec")
        .args(["--provider", "fixture"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    drop(child.stdin.take().expect("stdin"));
    child.wait_with_output().expect("wait euler")
}

fn exec_new(workspace: &Workspace, extra: &[&str]) -> Output {
    let log = workspace.log.to_str().expect("utf8 log");
    let mut args = vec!["--provenance", log];
    args.extend_from_slice(extra);
    args.push("hello");
    exec(workspace, &args)
}

fn exec_resume(workspace: &Workspace, extra: &[&str]) -> Output {
    let log = workspace.log.to_str().expect("utf8 log");
    let mut args = vec!["--resume", log];
    args.extend_from_slice(extra);
    args.push("again");
    exec(workspace, &args)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The `execution_profile` of every event of `kind`, in log order.
fn recorded_profiles(log: &Path, kind: &str) -> Vec<serde_json::Value> {
    fs::read_to_string(log)
        .expect("read log")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
        .filter(|event| event["kind"] == kind)
        .map(|event| event["payload"]["execution_profile"].clone())
        .collect()
}

#[test]
fn built_in_headless_default_records_the_workspace_preset() {
    let workspace = workspace();
    let output = exec_new(&workspace, &[]);
    assert!(output.status.success(), "{}", stderr(&output));

    let profiles = recorded_profiles(&workspace.log, "session.start");
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0]["name"], "workspace");
    assert_eq!(profiles[0]["fields"]["sandbox"], "on");
    assert_eq!(profiles[0]["fields"]["network"], "on");
    assert_eq!(profiles[0]["fields"]["approvals"], "escalation-only");
    assert_eq!(profiles[0]["fields"]["reviewer"], "user");
    assert!(profiles[0]["fields_sha256"].is_string());
}

#[test]
fn recorded_fields_digest_recomputes_from_the_logged_fields() {
    let workspace = workspace();
    let output = exec_new(&workspace, &[]);
    assert!(output.status.success(), "{}", stderr(&output));

    // The events contract: SHA-256 over the compact JSON of `fields` with
    // keys sorted.
    let profiles = recorded_profiles(&workspace.log, "session.start");
    let fields: BTreeMap<String, serde_json::Value> =
        serde_json::from_value(profiles[0]["fields"].clone()).expect("fields object");
    let digest = Sha256::digest(serde_json::to_vec(&fields).expect("serialize"));
    assert_eq!(profiles[0]["fields_sha256"], format!("{digest:x}"));
}

#[test]
fn headless_default_selects_a_user_profile_and_the_flag_overrides_it() {
    let workspace = workspace();
    write_profiles(
        &workspace,
        r#"{
            "defaults": {"headless": "ci"},
            "profiles": {"ci": {"extends": "locked", "approvals": "always"}}
        }"#,
    );
    let output = exec_new(&workspace, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        recorded_profiles(&workspace.log, "session.start")[0]["name"],
        "ci"
    );

    fs::remove_file(&workspace.log).expect("remove log");
    let output = exec_new(&workspace, &["--profile", "locked"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        recorded_profiles(&workspace.log, "session.start")[0]["name"],
        "locked"
    );
}

/// The profile's reviewer is the session's reviewer unless
/// `--permission-reviewer` overrides it; the record says which one ran.
#[test]
fn profile_reviewer_applies_and_the_reviewer_flag_overrides_it() {
    let workspace = workspace();
    write_profiles(
        &workspace,
        r#"{"profiles": {"ci": {"extends": "locked", "reviewer": "guardian"}}}"#,
    );
    for (extra, expected) in [
        (&["--profile", "ci"][..], "guardian"),
        (
            &["--profile", "ci", "--permission-reviewer", "user"][..],
            "user",
        ),
    ] {
        let output = exec_new(&workspace, extra);
        assert!(output.status.success(), "{}", stderr(&output));
        let start = fs::read_to_string(&workspace.log)
            .expect("read log")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
            .find(|event| event["kind"] == "session.start")
            .expect("session.start");
        assert_eq!(start["payload"]["permission_reviewer"], expected);
        assert_eq!(
            start["payload"]["execution_profile"]["fields"]["reviewer"],
            expected
        );
        fs::remove_file(&workspace.log).expect("remove log");
    }
}

#[test]
fn subagent_default_naming_a_profile_warns_that_it_is_not_applied() {
    let workspace = workspace();
    write_profiles(&workspace, r#"{"defaults": {"subagent": "locked"}}"#);
    let output = exec_new(&workspace, &["--profile", "full"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(
            "warning: defaults.subagent \"locked\" is not applied yet; child agents run under \
             the session's execution profile"
        ),
        "{}",
        stderr(&output)
    );

    write_profiles(&workspace, r#"{"defaults": {"subagent": "inherit"}}"#);
    let output = exec_resume(&workspace, &["--profile", "full"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        !stderr(&output).contains("defaults.subagent"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn unknown_profile_names_are_startup_errors() {
    let workspace = workspace();
    let output = exec_new(&workspace, &["--profile", "fulll"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains(
            "unknown execution profile \"fulll\"; defined profiles: full, workspace, locked"
        ),
        "{}",
        stderr(&output)
    );
    assert!(!workspace.log.exists(), "no session starts");
}

/// A scripted provider turn whose one command writes its `HOME` to
/// `home.txt`, as the `--provider-option` that selects it.
fn home_probe_script(workspace: &Workspace) -> String {
    let script = workspace.root.join("script.json");
    fs::write(
        &script,
        serde_json::json!({
            "version": 1,
            "responses": [
                {"events": [
                    {"tool_call": {
                        "id": "call-home",
                        "name": "run_shell",
                        "input": {"command": "printf %s \"$HOME\" > home.txt"}
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
    )
    .expect("write script");
    format!("event-script={}", script.display())
}

/// `--profile workspace` runs a command without a prompt even under exec's
/// default read-only tier (ADR 0026 row P), in a private `HOME` that, with
/// no session store behind `--provenance`, lives under the Euler home's
/// `state/homes` and is removed when the session ends.
#[test]
fn the_workspace_profile_runs_commands_in_a_private_home_without_a_prompt() {
    let workspace = workspace();
    let log = workspace.log.to_str().expect("utf8 log");
    let event_script = home_probe_script(&workspace);
    let output = exec(
        &workspace,
        &[
            "--provider-option",
            &event_script,
            "--provenance",
            log,
            "--profile",
            "workspace",
            "--extensions",
            "none",
            "--project-context",
            "off",
            "work",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let events: Vec<serde_json::Value> = fs::read_to_string(&workspace.log)
        .expect("read log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("json line"))
        .collect();
    let start = events
        .iter()
        .find(|event| event["kind"] == "session.start")
        .expect("session.start");
    assert_eq!(start["payload"]["execution_profile"]["name"], "workspace");
    assert_eq!(
        start["payload"]["execution_profile"]["fields"]["network"],
        "on"
    );
    let result = events
        .iter()
        .find(|event| event["kind"] == "tool.result")
        .expect("tool.result");
    let error = result["payload"]["error"].as_str().unwrap_or_default();
    assert!(!error.contains("permission denied"), "{error}");
    if start["payload"]["sandbox_backend"] == "unavailable" || result["payload"]["ok"] != true {
        // No backend on this host: the command failed closed, which the
        // approvals rule above does not depend on.
        return;
    }
    let home = PathBuf::from(
        fs::read_to_string(workspace.root.join("home.txt")).expect("command wrote its HOME"),
    );
    let homes = workspace
        .home
        .join(".euler/state/homes")
        .canonicalize()
        .expect("homes directory");
    assert!(home.starts_with(&homes), "{}", home.display());
    assert!(!home.exists(), "the fallback HOME outlived its session");
}

/// The first `tool.result` payload of the log.
fn first_tool_result(log: &Path) -> serde_json::Value {
    fs::read_to_string(log)
        .expect("read log")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
        .find(|event| event["kind"] == "tool.result")
        .expect("tool.result")["payload"]
        .clone()
}

/// The built-in headless default is `workspace` (ADR 0026 step 7), so plain
/// `euler exec` runs a command without a prompt, and `--profile locked`
/// restores the read-only tier's denial (resolved question 11).
#[test]
fn headless_default_runs_commands_and_the_locked_profile_restores_read_only() {
    let workspace = workspace();
    let log = workspace.log.to_str().expect("utf8 log");
    let event_script = home_probe_script(&workspace);
    let run = |extra: &[&str]| {
        let _ = fs::remove_file(&workspace.log);
        let mut args = vec![
            "--provider-option",
            &event_script,
            "--provenance",
            log,
            "--extensions",
            "none",
            "--project-context",
            "off",
        ];
        args.extend_from_slice(extra);
        args.push("work");
        let output = exec(&workspace, &args);
        assert!(output.status.success(), "{}", stderr(&output));
        (
            session_start(&workspace.log),
            first_tool_result(&workspace.log),
        )
    };

    let (start, result) = run(&[]);
    assert_eq!(start["execution_profile"]["name"], "workspace");
    // Without a backend the profile lifts no approval and the command fails
    // closed.
    let backend_enforced = start["sandbox_backend"] != "unavailable";
    assert_eq!(result["ok"], backend_enforced, "{result}");

    let (start, result) = run(&["--profile", "locked"]);
    assert_eq!(start["execution_profile"]["name"], "locked");
    assert_eq!(result["ok"], false, "{result}");
    let error = result["error"].as_str().unwrap_or_default();
    assert!(error.contains("denied"), "{error}");
}

/// `--auto-approve` under a profile that already runs commands and writes
/// is a no-op with a one-line notice naming the profile (ADR 0026 resolved
/// question 2); without the flag, or under `locked`, there is no notice.
#[test]
fn auto_approve_under_a_profile_that_covers_it_prints_a_notice() {
    let workspace = workspace();
    let notice = "note: --auto-approve has no effect: execution profile workspace already \
                  allows what it asks for";

    let output = exec_new(&workspace, &["--auto-approve", "read-only"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains(notice), "{}", stderr(&output));
    assert_eq!(
        stderr(&output).matches("--auto-approve").count(),
        1,
        "{}",
        stderr(&output)
    );

    fs::remove_file(&workspace.log).expect("remove log");
    let output = exec_new(&workspace, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        !stderr(&output).contains("--auto-approve"),
        "{}",
        stderr(&output)
    );

    fs::remove_file(&workspace.log).expect("remove log");
    let output = exec_new(
        &workspace,
        &["--profile", "locked", "--auto-approve", "read-only"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        !stderr(&output).contains("--auto-approve"),
        "{}",
        stderr(&output)
    );
}

/// `--auto-approve trusted-local` disables `auto` project context admission
/// only under a profile with `approvals: always` (ADR 0026 resolved
/// question 10); under `workspace` the acknowledgment rule decides, so an
/// acknowledged digest is admitted.
#[test]
fn trusted_local_disables_project_context_only_under_approvals_always() {
    let workspace = workspace();
    fs::write(workspace.root.join("EULER.md"), "# Guidance\n").expect("write EULER.md");
    let reason = |extra: &[&str]| {
        let _ = fs::remove_file(&workspace.log);
        let mut args = vec!["--auto-approve", "trusted-local"];
        args.extend_from_slice(extra);
        let output = exec_new(&workspace, &args);
        assert!(output.status.success(), "{}", stderr(&output));
        session_start(&workspace.log)["project_context"]["resolution_reason"].clone()
    };

    assert_eq!(reason(&[]), "no_acknowledgment");
    assert_eq!(reason(&["--profile", "locked"]), "trusted_local_auto_off");

    // Record the acknowledgment the interactive surface would write for the
    // digest the last run observed.
    let snapshot = fs::read_to_string(&workspace.log)
        .expect("read log")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
        .find(|event| event["kind"] == "project.context.snapshot")
        .expect("project.context.snapshot")["payload"]
        .clone();
    euler_core::AcknowledgmentStore::new(workspace.home.join(".euler"))
        .record(
            &workspace.root.canonicalize().expect("canonical root"),
            snapshot["workspace_identity"]["digest"]
                .as_str()
                .expect("identity digest"),
            snapshot["candidate_digest"]
                .as_str()
                .expect("candidate digest"),
        )
        .expect("record acknowledgment");

    assert_eq!(reason(&[]), "acknowledged");
    assert_eq!(reason(&["--profile", "locked"]), "trusted_local_auto_off");
}

/// Headless `exec` cannot answer an escalation, so under `--profile
/// workspace` it denies one with a tool error naming the profile and the
/// flag that would allow it (ADR 0026 section 7), and the command does not
/// run.
#[test]
fn headless_exec_denies_an_escalation_naming_the_profile_and_flag() {
    let workspace = workspace();
    let log = workspace.log.to_str().expect("utf8 log");
    let script = workspace.root.join("script.json");
    fs::write(
        &script,
        serde_json::json!({
            "version": 1,
            "responses": [
                {"events": [
                    {"tool_call": {
                        "id": "call-host",
                        "name": "run_shell",
                        "input": {
                            "command": "touch escalated.txt",
                            "escalate": {"reason": "needs the host"}
                        }
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
    )
    .expect("write script");
    let event_script = format!("event-script={}", script.display());
    let output = exec(
        &workspace,
        &[
            "--provider-option",
            &event_script,
            "--provenance",
            log,
            "--profile",
            "workspace",
            "--extensions",
            "none",
            "--project-context",
            "off",
            "work",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!workspace.root.join("escalated.txt").exists());

    let events: Vec<serde_json::Value> = fs::read_to_string(&workspace.log)
        .expect("read log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("json line"))
        .collect();
    let start = events
        .iter()
        .find(|event| event["kind"] == "session.start")
        .expect("session.start");
    if start["payload"]["sandbox_backend"] == "unavailable" {
        // No backend on this host: the read-only tier keeps shell-exec
        // denied outright, before the profile could route the ask.
        return;
    }
    let result = events
        .iter()
        .find(|event| event["kind"] == "tool.result")
        .expect("tool.result");
    let error = result["payload"]["error"].as_str().expect("error");
    assert!(
        error.contains(
            "execution profile `workspace` asks before running a command outside the sandbox"
        ),
        "{error}"
    );
    assert!(error.contains("--permission-reviewer guardian"), "{error}");
}

/// With the Euler home's session store, the `workspace` profile's private
/// `HOME` is `<euler home>/sessions/<id>/home` and outlives the process, so
/// a `pip install --user` persists across a resume.
#[test]
fn the_workspace_profile_keeps_its_home_in_the_session_store() {
    let workspace = workspace();
    let event_script = home_probe_script(&workspace);
    let output = exec(
        &workspace,
        &[
            "--provider-option",
            &event_script,
            "--profile",
            "workspace",
            "--extensions",
            "none",
            "--project-context",
            "off",
            "work",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let Ok(home) = fs::read_to_string(workspace.root.join("home.txt")) else {
        // No backend on this host: the command failed closed.
        return;
    };
    let home = PathBuf::from(home);
    let sessions = workspace
        .home
        .join(".euler/sessions")
        .canonicalize()
        .expect("session store");
    assert_eq!(
        home.file_name().and_then(|name| name.to_str()),
        Some("home")
    );
    assert_eq!(
        home.parent().and_then(Path::parent),
        Some(sessions.as_path()),
        "{}",
        home.display()
    );
    assert!(home.is_dir(), "the session HOME was removed at exit");
}

#[test]
fn malformed_profiles_json_names_the_file_and_field() {
    let workspace = workspace();
    let path = write_profiles(
        &workspace,
        r#"{"profiles": {"ci": {"extends": "locked", "netwrk": "off"}}}"#,
    );
    let output = exec_new(&workspace, &[]);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains(&path.display().to_string()), "{message}");
    assert!(
        message.contains("profile \"ci\": unknown field `netwrk`"),
        "{message}"
    );
}

#[test]
fn project_overlay_can_tighten_but_never_loosen() {
    let workspace = workspace();
    write_overlay(&workspace, r#"{"approvals": "always"}"#);
    let output = exec_new(&workspace, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output)
            .contains("execution profile: workspace, tightened by .euler/profile-overlay.json"),
        "{}",
        stderr(&output)
    );

    fs::remove_file(&workspace.log).expect("remove log");
    write_overlay(&workspace, r#"{"network": "on"}"#);
    let output = exec_new(&workspace, &["--profile", "locked"]);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("profile-overlay.json"), "{message}");
    assert!(
        message.contains("network \"on\" is looser than the profile's \"off\""),
        "{message}"
    );

    // A valid tightening of `full` that this build cannot enforce names the
    // overlay and the way out.
    write_overlay(&workspace, r#"{"network": "off"}"#);
    let output = exec_new(&workspace, &["--profile", "full"]);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(
        message.contains(
            "the project overlay .euler/profile-overlay.json sets network on profile \"full\""
        ),
        "{message}"
    );
    assert!(message.contains("--profile locked"), "{message}");
}

#[test]
fn resume_records_the_profile_it_runs_under_and_shows_a_change() {
    let workspace = workspace();
    write_profiles(
        &workspace,
        r#"{"defaults": {"headless": "ci"}, "profiles": {"ci": {"extends": "locked"}}}"#,
    );
    let output = exec_new(&workspace, &[]);
    assert!(output.status.success(), "{}", stderr(&output));

    // Same fields under another name: not looser, so the resolved profile
    // applies, and the change is shown and recorded.
    write_profiles(&workspace, r#"{"defaults": {"headless": "locked"}}"#);
    let output = exec_resume(&workspace, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("execution profile: locked (recorded: ci)"),
        "{}",
        stderr(&output)
    );
    let resumed = recorded_profiles(&workspace.log, "session.resumed");
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0]["name"], "locked");
}

/// A session started under `locked` (the headless default before ADR 0026
/// step 7) resumes under `locked`: the `workspace` default is looser, so the
/// recorded profile is kept and named on stderr, and `--auto-approve` applies
/// its tier without the profile notice.
#[test]
fn resume_of_a_locked_session_keeps_locked_under_the_workspace_default() {
    let workspace = workspace();
    let output = exec_new(&workspace, &["--profile", "locked"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let output = exec_resume(&workspace, &["--auto-approve", "trusted-local"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("kept the recorded profile locked"),
        "{}",
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains("--auto-approve"),
        "{}",
        stderr(&output)
    );
    let resumed = recorded_profiles(&workspace.log, "session.resumed");
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0]["name"], "locked");
}

/// `--permission-reviewer` applies before the resume rule compares, so the
/// same flag on start and resume shows no change, and dropping it names the
/// reviewer change.
#[test]
fn resume_with_the_same_reviewer_flag_shows_no_profile_change() {
    let workspace = workspace();
    let guardian = ["--permission-reviewer", "guardian"];
    let output = exec_new(&workspace, &guardian);
    assert!(output.status.success(), "{}", stderr(&output));

    let output = exec_resume(&workspace, &guardian);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        !stderr(&output).contains("execution profile:"),
        "{}",
        stderr(&output)
    );

    let output = exec_resume(&workspace, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(
            "execution profile: workspace (recorded: workspace; changed reviewer guardian -> user)"
        ),
        "{}",
        stderr(&output)
    );
}

/// Profile errors fail a resume before the relocation it accepted is
/// appended to the log.
#[test]
fn resume_profile_errors_fail_before_relocation_is_recorded() {
    let workspace = workspace();
    let output = exec_new(&workspace, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let moved = workspace.root.with_file_name("moved");
    fs::create_dir_all(moved.join(".git")).expect("second repository");
    write_profiles(
        &workspace,
        r#"{"profiles": {"ci": {"extends": "locked", "bogus": 1}}}"#,
    );
    let before = fs::read_to_string(&workspace.log).expect("read log");

    let log = workspace.log.to_str().expect("utf8 log");
    let output = exec_in(
        &workspace,
        &moved,
        &["--resume", log, "--accept-relocation", "again"],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("profiles.json"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(&workspace.log).expect("read log"),
        before,
        "nothing is appended"
    );
}

#[test]
fn resume_of_a_log_without_a_profile_resolves_as_a_new_session() {
    let workspace = workspace();
    let output = exec_new(&workspace, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    // Strip the profile to get the shape a log written before profiles has.
    let content = fs::read_to_string(&workspace.log).expect("read log");
    let mut legacy = String::new();
    for line in content.lines() {
        let mut event: serde_json::Value = serde_json::from_str(line).expect("json line");
        if event["kind"] == "session.start" {
            let payload = event["payload"].as_object_mut().expect("payload");
            payload.remove("execution_profile");
            // Recompute the runtime projection digest the way an older
            // writer did: over the payload without `runtime`.
            let mut runtime = payload.remove("runtime").expect("runtime identity");
            let digest = Sha256::digest(serde_json::to_vec(payload).expect("serialize"));
            runtime["session_start_projection_sha256"] = format!("{digest:x}").into();
            payload.insert("runtime".to_owned(), runtime);
        }
        legacy.push_str(&serde_json::to_string(&event).expect("serialize"));
        legacy.push('\n');
    }
    let mut file = fs::File::create(&workspace.log).expect("rewrite log");
    file.write_all(legacy.as_bytes()).expect("write log");

    let output = exec_resume(&workspace, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("execution profile: workspace (this session recorded no profile)"),
        "{}",
        stderr(&output)
    );
}

/// A fixture script that makes one `run_shell` call, then finishes.
fn shell_script(workspace: &Workspace, command: &str) -> String {
    tool_script(
        workspace,
        "run_shell",
        serde_json::json!({ "command": command }),
    )
}

/// A fixture script that makes one call to tool `name`, then finishes.
fn tool_script(workspace: &Workspace, name: &str, input: serde_json::Value) -> String {
    let script = workspace.home.join("tool-script.json");
    let body = serde_json::json!({
        "version": 1,
        "responses": [
            {"events": [
                {"tool_call": {"id": "call-tool", "name": name, "input": input}},
                {"finished": {"stop_reason": "tool_use"}}
            ]},
            {"events": [
                {"text_delta": "done"},
                {"finished": {"stop_reason": "completed"}}
            ]}
        ]
    });
    fs::write(&script, body.to_string()).expect("write script");
    format!("event-script={}", script.display())
}

fn session_start(log: &Path) -> serde_json::Value {
    fs::read_to_string(log)
        .expect("read log")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
        .find(|event| event["kind"] == "session.start")
        .expect("session.start")["payload"]
        .clone()
}

#[test]
fn exec_under_full_runs_shell_on_the_host_without_asking() {
    let workspace = workspace();
    let script = shell_script(&workspace, "printf host-ran > out.txt");
    let output = exec_new(
        &workspace,
        &["--profile", "full", "--provider-option", &script],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    assert_eq!(
        fs::read_to_string(workspace.root.join("out.txt")).expect("written"),
        "host-ran"
    );
    let start = session_start(&workspace.log);
    assert_eq!(start["execution_profile"]["name"], "full");
    assert_eq!(start["sandbox_backend"], "host");
    assert_eq!(start["command_approvals"], "off");
    assert!(
        !stderr(&output).contains("--auto-approve"),
        "no notice without the flag: {}",
        stderr(&output)
    );
}

#[test]
fn exec_under_full_withholds_variables_providers_json_references() {
    let workspace = workspace();
    fs::write(
        workspace.home.join(".euler").join("providers.json"),
        r#"{"version": 1, "providers": {"proxy": {
            "api_family": "openai_chat_completions",
            "base_url": "https://litellm.example.com/v1",
            "api_key": "$MY_PROXY_KEY",
            "models": [{"id": "proxy-model"}]
        }}}"#,
    )
    .expect("write providers.json");
    let script = shell_script(
        &workspace,
        "printf '[%s][%s]' \"$MY_PROXY_KEY\" \"$UNRELATED_SETTING\" > out.txt",
    );
    let log = workspace.log.to_str().expect("utf8 log");
    let output = exec_with_env(
        &workspace,
        &workspace.root,
        &[
            ("MY_PROXY_KEY", "proxy-key-value"),
            ("UNRELATED_SETTING", "kept"),
        ],
        &[
            "--provenance",
            log,
            "--profile",
            "full",
            "--provider-option",
            &script,
            "hello",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(workspace.root.join("out.txt")).expect("written"),
        "[][kept]"
    );
}

/// Whether any request in the log asked a human (`permission.prompt`).
fn asked_permission(log: &Path) -> bool {
    fs::read_to_string(log)
        .expect("read log")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
        .any(|event| event["kind"] == "permission.prompt")
}

/// Run one `write_file` to `target` under `full`; true when it asked and
/// never landed (headless exec denies every ask).
fn full_asks_before_writing(workspace: &Workspace, target: &Path) -> bool {
    let script = tool_script(
        workspace,
        "write_file",
        serde_json::json!({
            "path": target.to_str().expect("utf8 path"),
            "content": "{}",
        }),
    );
    let output = exec_new(
        workspace,
        &["--profile", "full", "--provider-option", &script],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    asked_permission(&workspace.log) && !target.exists()
}

#[test]
fn exec_under_full_asks_before_writing_euler_secrets_and_trust_records() {
    let unguarded: Vec<&str> = [
        "providers.json",
        "auth.json",
        "profiles.json",
        "user-grants.json",
        "project-grants/root.json",
        "project-context/root.json",
        "extensions/state.jsonl",
        "extensions/links.json",
        "extensions/linked-activation.jsonl",
    ]
    .into_iter()
    .filter(|relative| {
        let workspace = workspace();
        let target = workspace.home.join(".euler").join(relative);
        // A write into a missing directory fails before it lands, so give
        // each target the parent it has once Euler has used it.
        fs::create_dir_all(target.parent().expect("parent")).expect("parent dir");
        !full_asks_before_writing(&workspace, &target)
    })
    .collect();
    assert!(unguarded.is_empty(), "wrote without asking: {unguarded:?}");
}

#[cfg(unix)]
#[test]
fn exec_under_full_asks_before_writing_the_auth_file_through_a_skills_symlink() {
    let workspace = workspace();
    let euler = workspace.home.join(".euler");
    let auth = euler.join("auth.json");
    fs::write(&auth, "{}").expect("auth file");
    let link = euler.join("skills").join("note");
    fs::create_dir_all(link.parent().expect("parent")).expect("skills dir");
    std::os::unix::fs::symlink(&auth, &link).expect("symlink");
    let script = tool_script(
        &workspace,
        "write_file",
        serde_json::json!({
            "path": link.to_str().expect("utf8 path"),
            "content": "{\"stolen\": true}",
        }),
    );
    let output = exec_new(
        &workspace,
        &["--profile", "full", "--provider-option", &script],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        asked_permission(&workspace.log),
        "the symlinked write did not ask"
    );
    assert_eq!(fs::read_to_string(&auth).expect("auth file"), "{}");
}

#[test]
fn exec_under_full_writes_a_skill_into_the_euler_home_without_asking() {
    let workspace = workspace();
    let skill = workspace
        .home
        .join(".euler")
        .join("skills")
        .join("notes")
        .join("SKILL.md");
    fs::create_dir_all(skill.parent().expect("parent")).expect("skill dir");
    let script = tool_script(
        &workspace,
        "write_file",
        serde_json::json!({
            "path": skill.to_str().expect("utf8 path"),
            "content": "---\nname: notes\ndescription: Take notes.\n---\nTake notes.\n",
        }),
    );
    let output = exec_new(
        &workspace,
        &["--profile", "full", "--provider-option", &script],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(skill.exists(), "the skill write did not land");
    assert!(!asked_permission(&workspace.log), "the skill write asked");
}

#[test]
fn auto_approve_under_full_is_a_noop_with_a_notice() {
    let workspace = workspace();
    let script = shell_script(&workspace, "printf host-ran > out.txt");
    let output = exec_new(
        &workspace,
        &[
            "--profile",
            "full",
            "--auto-approve",
            "read-only",
            "--provider-option",
            &script,
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(
            "--auto-approve has no effect: execution profile full already allows what it asks for"
        ),
        "{}",
        stderr(&output)
    );
    // The read-only tier did not tighten the profile.
    assert!(workspace.root.join("out.txt").exists());
}

#[test]
fn exec_under_full_lists_background_jobs_still_running_at_exit() {
    let workspace = workspace();
    let script = shell_script(&workspace, "sleep 30 > /dev/null 2>&1 &");
    let output = exec_new(
        &workspace,
        &["--profile", "full", "--provider-option", &script],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("Background jobs Euler started are still running:"),
        "{text}"
    );
    let line = text
        .lines()
        .find(|line| line.contains(": sleep 30 > /dev/null 2>&1 &"))
        .unwrap_or_else(|| panic!("job listed: {text}"));
    let group: i32 = line
        .trim()
        .strip_prefix("process group ")
        .and_then(|rest| rest.split(':').next())
        .and_then(|group| group.parse().ok())
        .expect("group id");
    // Euler left the job running; end it so the test leaves nothing behind.
    let killed = Command::new("kill")
        .args(["-KILL", "--", &format!("-{group}")])
        .status()
        .expect("kill");
    assert!(killed.success());
}

#[test]
fn interactive_default_is_full_and_no_picker_runs_without_a_terminal() {
    let workspace = workspace();
    let log = workspace.log.to_str().expect("utf8 log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_euler"))
        .env("HOME", &workspace.home)
        .env_remove("EULER_HOME")
        .env_remove("EULER_PROVIDER")
        .env_remove("EULER_MODEL")
        .current_dir(&workspace.root)
        .args(["--provider", "fixture", "--provenance", log])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn euler");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"hello\n")
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait euler");
    assert!(output.status.success(), "{}", stderr(&output));

    let start = session_start(&workspace.log);
    assert_eq!(start["execution_profile"]["name"], "full");
    assert_eq!(start["sandbox_backend"], "host");
    assert!(
        !workspace.home.join(".euler").join("profiles.json").exists(),
        "the first-run picker needs a terminal and writes nothing without one"
    );
}
