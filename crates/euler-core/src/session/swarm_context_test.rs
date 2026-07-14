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

#[test]
fn selector_fields_are_rejected_not_silently_defaulted() {
    // The extension-run bridge used to parse these with closures that fell
    // back to defaults on garbage, while the tool seam rejected it. One field
    // must not mean two things depending on which seam the caller entered.
    let cases = [
        (
            json!({"prompt": "p", "staged": "yes"}),
            "staged must be boolean",
        ),
        (
            json!({"prompt": "p", "max_file_bytes": 0}),
            "max_file_bytes must be a positive integer",
        ),
        (
            json!({"prompt": "p", "max_file_bytes": "big"}),
            "max_file_bytes must be a positive integer",
        ),
        (json!({"prompt": "p", "base": 7}), "base must be a string"),
        (
            json!({"prompt": "p", "files": ["a.rs", 3]}),
            "files must contain only strings",
        ),
        (
            json!({"prompt": "p", "files": "a.rs"}),
            "files must be an array of strings",
        ),
        (
            json!({"prompt": "p", "mode": "astrology"}),
            "mode must be plan, review-code, review-diff, or review-pr",
        ),
    ];
    for (input, expected) in cases {
        let object = input.as_object().expect("object");
        let error = request_from_object(object).expect_err("must reject");
        assert_eq!(error, expected, "input {input}");
    }
}

#[test]
fn each_mode_declares_the_authority_it_uses() {
    // Assembly reads files and runs git/gh. Those are separate authority from
    // the AgentSpawn the tool itself is gated on.
    assert_eq!(ReviewMode::Plan.required_capabilities(), &[]);
    assert_eq!(
        ReviewMode::Code.required_capabilities(),
        &[Capability::FsRead]
    );
    assert_eq!(
        ReviewMode::Diff.required_capabilities(),
        &[Capability::ShellExec]
    );
    assert_eq!(
        ReviewMode::PullRequest.required_capabilities(),
        &[Capability::ShellExec, Capability::Network]
    );
}

#[test]
fn prompt_is_bounded_once_at_one_limit() {
    // A focus that parses at the tool seam but dies inside assemble would
    // reject the caller twice with two different explanations.
    let temp = tempfile::tempdir().expect("temp");
    let mut oversized = request(ReviewMode::Plan, "");
    oversized.prompt = "p".repeat(MAX_PROMPT_BYTES + 1);
    assert!(assemble(temp.path(), &oversized)
        .expect_err("oversized prompt")
        .contains(&format!("the limit is {MAX_PROMPT_BYTES}")));

    let mut at_limit = request(ReviewMode::Plan, "");
    at_limit.prompt = "p".repeat(MAX_PROMPT_BYTES);
    assemble(temp.path(), &at_limit).expect("a prompt at the limit must fit");
}

#[test]
fn empty_prompt_is_rejected() {
    let temp = tempfile::tempdir().expect("temp");
    assert!(assemble(temp.path(), &request(ReviewMode::Plan, "   "))
        .expect_err("blank prompt")
        .contains("review question or focus"));
}
