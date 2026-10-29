//! Execution profile selection, validation, and recording through the CLI
//! (ADR 0026 rollout steps 3 and 5: profile plumbing, the `locked` preset,
//! and the `workspace` preset).

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
    let mut child = Command::new(env!("CARGO_BIN_EXE_euler"))
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
fn built_in_default_records_the_locked_preset() {
    let workspace = workspace();
    let output = exec_new(&workspace, &[]);
    assert!(output.status.success(), "{}", stderr(&output));

    let profiles = recorded_profiles(&workspace.log, "session.start");
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0]["name"], "locked");
    assert_eq!(profiles[0]["fields"]["sandbox"], "on");
    assert_eq!(profiles[0]["fields"]["network"], "off");
    assert_eq!(profiles[0]["fields"]["approvals"], "always");
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
fn presets_not_shipped_yet_and_unknown_names_are_startup_errors() {
    let workspace = workspace();
    for (profile, expected) in [
        (
            "full",
            "the full preset is not available in this version yet",
        ),
        (
            "fulll",
            "unknown execution profile \"fulll\"; defined profiles: full, workspace, locked",
        ),
    ] {
        let output = exec_new(&workspace, &["--profile", profile]);
        assert!(!output.status.success());
        assert!(stderr(&output).contains(expected), "{}", stderr(&output));
        assert!(!workspace.log.exists(), "no session starts");
    }
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
            .contains("execution profile: locked, tightened by .euler/profile-overlay.json"),
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
    write_profiles(&workspace, "{}");
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
            "execution profile: locked (recorded: locked; changed reviewer guardian -> user)"
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
        stderr(&output).contains("execution profile: locked (this session recorded no profile)"),
        "{}",
        stderr(&output)
    );
}
