use super::*;
use std::fs;

fn request(mode: ReviewMode, prompt: &str) -> ContextRequest {
    ContextRequest {
        mode,
        prompt: prompt.to_owned(),
        context: None,
        files: Vec::new(),
        base: None,
        staged: false,
        pr: None,
        current: false,
        include_full_files: false,
        include_comments: false,
        max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        max_total_bytes: 32 * 1024,
        max_diff_bytes: 24 * 1024,
    }
}

#[test]
fn plan_uses_only_explicit_context_and_prompt() {
    let temp = tempfile::tempdir().expect("temp");
    let mut request = request(ReviewMode::Plan, "find design gaps");
    request.context = Some("step one\nstep two".to_owned());
    let assembled = assemble(temp.path(), &request).expect("assemble");
    assert!(assembled.body.contains("step one"));
    assert!(assembled.body.contains("find design gaps"));
    assert_eq!(assembled.manifest["mode"], json!("plan"));
    assert_eq!(assembled.body.matches("step one").count(), 1);
}

#[test]
fn code_mode_reads_bounded_files_and_reports_skips() {
    let temp = tempfile::tempdir().expect("temp");
    fs::write(temp.path().join("small.rs"), "fn small() {}\n").expect("small");
    fs::write(temp.path().join("large.rs"), "x".repeat(100)).expect("large");
    let mut request = request(ReviewMode::Code, "review files");
    request.files = vec!["small.rs".to_owned(), "large.rs".to_owned()];
    request.max_file_bytes = 32;
    let assembled = assemble(temp.path(), &request).expect("assemble");
    assert!(assembled.body.contains("fn small"));
    assert!(!assembled.body.contains(&"x".repeat(100)));
    assert!(assembled.manifest["skipped"]
        .as_array()
        .expect("skipped")
        .iter()
        .any(|item| item.as_str().is_some_and(|item| item.contains("large.rs"))));
}

#[test]
fn code_mode_rejects_escape_and_symlink_escape() {
    let temp = tempfile::tempdir().expect("temp");
    let mut request = request(ReviewMode::Code, "review files");
    request.files = vec!["../secret".to_owned()];
    assert!(assemble(temp.path(), &request)
        .expect_err("traversal")
        .contains("parent traversal"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink("/etc/passwd", temp.path().join("outside")).expect("symlink");
        request.files = vec!["outside".to_owned()];
        assert!(assemble(temp.path(), &request)
            .expect_err("escape")
            .contains("outside the repository"));
    }
}

#[test]
fn diff_mode_assembles_working_tree_patch() {
    let temp = tempfile::tempdir().expect("temp");
    let root = temp.path();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(root)
        .status()
        .expect("git")
        .success());
    fs::write(root.join("a.txt"), "old\n").expect("write");
    assert!(Command::new("git")
        .args(["add", "a.txt"])
        .current_dir(root)
        .status()
        .expect("git")
        .success());
    assert!(Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            "init"
        ])
        .current_dir(root)
        .status()
        .expect("git")
        .success());
    fs::write(root.join("a.txt"), "new\n").expect("modify");
    let assembled =
        assemble(root, &request(ReviewMode::Diff, "find regressions")).expect("assemble");
    assert!(assembled.body.contains("-old"));
    assert!(assembled.body.contains("+new"));
}

#[test]
fn truncation_is_utf8_safe_and_manifested() {
    let temp = tempfile::tempdir().expect("temp");
    let mut request = request(ReviewMode::Plan, "r");
    request.context = Some("é".repeat(200_000));
    request.max_total_bytes = 256 * 1024;
    let assembled = assemble(temp.path(), &request).expect("assemble");
    assert!(assembled.manifest["truncated"].as_bool().expect("bool"));
    assert!(std::str::from_utf8(assembled.body.as_bytes()).is_ok());
}
