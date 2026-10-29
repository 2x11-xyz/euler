//! Execution profile selection, validation, and recording through the CLI
//! (ADR 0026 rollout step 3: profile plumbing and the `locked` preset).

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
    let mut child = Command::new(env!("CARGO_BIN_EXE_euler"))
        .env("HOME", &workspace.home)
        .env_remove("EULER_HOME")
        .env_remove("EULER_PROVIDER")
        .env_remove("EULER_MODEL")
        .current_dir(&workspace.root)
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
            "workspace",
            "the workspace preset is not available in this version yet",
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
