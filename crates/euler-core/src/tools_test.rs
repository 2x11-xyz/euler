use super::*;
use serde_json::json;
use std::env;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvRestore {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvRestore {
    fn capture(names: &[&'static str]) -> Self {
        Self {
            saved: names
                .iter()
                .map(|name| (*name, env::var_os(name)))
                .collect(),
        }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => env::set_var(*name, value),
                None => env::remove_var(*name),
            }
        }
    }
}

#[test]
fn newline_terminated_exact_fit_is_not_marked_truncated() {
    assert_eq!(bound_text("alpha\n", 6, 1), "alpha\n");
}

#[test]
fn read_file_without_offset_returns_fitting_file_unchanged() {
    let temp = tempfile::tempdir().expect("temp dir");
    let content = "alpha\nbeta\n";
    fs::write(temp.path().join("note.txt"), content).expect("fixture");
    let registry = ToolRegistry::new(temp.path());

    let execution = registry
        .execute("read_file", &json!({"path": "note.txt"}))
        .expect("read");

    assert_eq!(execution.output, content);
}

#[test]
fn read_file_offset_window_returns_requested_lines() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "one\ntwo\nthree\n").expect("fixture");
    let registry = ToolRegistry::new(temp.path());

    let execution = registry
        .execute(
            "read_file",
            &json!({"path": "note.txt", "offset": 2, "max_lines": 2}),
        )
        .expect("read");

    assert_eq!(execution.output, "two\nthree\n");
}

#[test]
fn read_file_without_trailing_newline_counts_and_reads_last_line() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "one\ntwo").expect("fixture");
    let registry = ToolRegistry::new(temp.path());

    let last_line = registry
        .execute("read_file", &json!({"path": "note.txt", "offset": 2}))
        .expect("read last line");
    let past_eof = registry
        .execute("read_file", &json!({"path": "note.txt", "offset": 3}))
        .expect("read past EOF");

    assert_eq!(last_line.output, "two");
    assert_eq!(past_eof.output, "[offset beyond EOF: total lines 2]");
}

#[test]
fn read_file_empty_file_returns_empty_output_without_marker() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("empty.txt"), "").expect("fixture");
    let registry = ToolRegistry::new(temp.path());

    let execution = registry
        .execute("read_file", &json!({"path": "empty.txt"}))
        .expect("read empty file");

    assert_eq!(execution.output, "");
}

#[test]
fn read_file_exact_byte_fit_with_offset_is_not_marked_truncated() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\nbeta\n").expect("fixture");
    let registry = ToolRegistry::new(temp.path());

    let execution = registry
        .execute(
            "read_file",
            &json!({"path": "note.txt", "offset": 2, "max_bytes": 5}),
        )
        .expect("read exact byte window");

    assert_eq!(execution.output, "beta\n");
}

#[test]
fn read_file_line_truncation_marker_names_continuation_offset_and_total() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "one\ntwo\nthree\nfour\n").expect("fixture");
    let registry = ToolRegistry::new(temp.path());

    let execution = registry
        .execute("read_file", &json!({"path": "note.txt", "max_lines": 2}))
        .expect("read");

    assert!(execution
        .output
        .contains("[truncated: showing lines 1-2 of 4; call read_file with offset=3 for more]"));
}

#[test]
fn read_file_byte_truncation_marker_names_last_full_line_and_continuation_offset() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "alpha\nbeta\ngamma\n").expect("fixture");
    let registry = ToolRegistry::new(temp.path());

    let execution = registry
        .execute("read_file", &json!({"path": "note.txt", "max_bytes": 8}))
        .expect("read");

    assert!(execution.output.contains("showing full lines 1-1 of 3"));
    assert!(execution.output.contains("plus partial line 2"));
    assert!(execution.output.contains(
        "line 2 is partial; call read_file with offset=2 and a larger max_bytes for the rest"
    ));
}

#[test]
fn read_file_does_not_claim_partial_line_when_no_bytes_are_emitted() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "éclair\n").expect("fixture");
    let registry = ToolRegistry::new(temp.path());

    let execution = registry
        .execute("read_file", &json!({"path": "note.txt", "max_bytes": 1}))
        .expect("read");

    assert_eq!(
        execution.output,
        "[truncated: showing no lines of 1; call read_file with offset=1 for more]"
    );
}

#[test]
fn read_file_offset_past_eof_reports_total_line_count() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "one\ntwo\n").expect("fixture");
    let registry = ToolRegistry::new(temp.path());

    let execution = registry
        .execute("read_file", &json!({"path": "note.txt", "offset": 10}))
        .expect("read");

    assert_eq!(execution.output, "[offset beyond EOF: total lines 2]");
}

#[test]
fn read_file_schema_includes_line_offset() {
    let registry = ToolRegistry::new(".");
    let read_file = registry
        .model_tools()
        .into_iter()
        .find(|tool| tool.name == "read_file")
        .expect("read_file schema");

    assert_eq!(read_file.parameters["properties"]["offset"]["minimum"], 1);
}

#[test]
fn read_file_rejects_zero_bounds() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "content").expect("fixture");
    let registry = ToolRegistry::new(temp.path());

    let max_bytes_error = registry
        .execute("read_file", &json!({"path": "note.txt", "max_bytes": 0}))
        .expect_err("zero max_bytes rejected");
    let max_lines_error = registry
        .execute("read_file", &json!({"path": "note.txt", "max_lines": 0}))
        .expect_err("zero max_lines rejected");

    assert!(matches!(
        max_bytes_error,
        ToolError::InvalidField("max_bytes")
    ));
    assert!(matches!(
        max_lines_error,
        ToolError::InvalidField("max_lines")
    ));
}

#[test]
fn edit_file_rejects_overlapping_replacement_matches() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("word.txt"), "banana").expect("fixture");
    let registry = ToolRegistry::new(temp.path());

    let error = registry
        .execute(
            "edit_file",
            &json!({"path": "word.txt", "old": "ana", "new": "ANA"}),
        )
        .expect_err("ambiguous edit");

    assert!(matches!(error, ToolError::ReplacementMatchCount(2)));
    assert_eq!(
        fs::read_to_string(temp.path().join("word.txt")).expect("read fixture"),
        "banana"
    );
}

#[test]
fn edit_file_creates_missing_file_when_old_is_empty() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());

    let execution = registry
        .execute(
            "edit_file",
            &json!({"path": "created.txt", "old": "", "new": "fresh\n"}),
        )
        .expect("create patch");
    registry
        .apply_patch(&execution.patch.expect("patch"))
        .expect("apply patch");

    assert_eq!(
        fs::read_to_string(temp.path().join("created.txt")).expect("created file"),
        "fresh\n"
    );
}

#[test]
fn edit_file_rejects_create_when_file_already_exists() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "existing").expect("fixture");
    let registry = ToolRegistry::new(temp.path());

    let error = registry
        .execute(
            "edit_file",
            &json!({"path": "note.txt", "old": "", "new": "fresh"}),
        )
        .expect_err("existing file rejected");

    assert!(matches!(error, ToolError::FileAlreadyExists));
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("read fixture"),
        "existing"
    );
}

#[test]
fn edit_file_rejects_create_when_parent_directory_is_missing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());

    let error = registry
        .execute(
            "edit_file",
            &json!({"path": "missing/created.txt", "old": "", "new": "fresh"}),
        )
        .expect_err("missing parent rejected");

    assert!(matches!(error, ToolError::ParentDirectoryMissing));
    assert!(!temp.path().join("missing").exists());
}

#[cfg(unix)]
#[test]
fn edit_file_rejects_create_through_symlink_escape() {
    let temp = tempfile::tempdir().expect("temp dir");
    let outside = tempfile::tempdir().expect("outside dir");
    symlink(outside.path(), temp.path().join("outside")).expect("symlink");
    let registry = ToolRegistry::new(temp.path());

    let error = registry
        .execute(
            "edit_file",
            &json!({"path": "outside/created.txt", "old": "", "new": "fresh"}),
        )
        .expect_err("escape rejected");

    assert!(matches!(
        error,
        ToolError::PathOutsideWorkspace {
            reason: "path escapes the workspace root",
            ..
        }
    ));
    assert!(!outside.path().join("created.txt").exists());
}

#[cfg(unix)]
#[test]
fn edit_file_rejects_create_through_broken_leaf_symlink_escape() {
    let temp = tempfile::tempdir().expect("temp dir");
    let outside = tempfile::tempdir().expect("outside dir");
    let outside_target = outside.path().join("target.txt");
    symlink(&outside_target, temp.path().join("escape.txt")).expect("symlink");
    let registry = ToolRegistry::new(temp.path());

    let error = registry
        .execute(
            "edit_file",
            &json!({"path": "escape.txt", "old": "", "new": "fresh"}),
        )
        .expect_err("broken symlink escape rejected");

    assert!(matches!(
        error,
        ToolError::PathOutsideWorkspace {
            reason: "path is a symlink whose target cannot be verified inside the workspace",
            ..
        }
    ));
    assert!(!outside_target.exists());
}

#[test]
fn edit_file_rejects_missing_file_when_old_is_non_empty() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());

    let error = registry
        .execute(
            "edit_file",
            &json!({"path": "missing.txt", "old": "needle", "new": "fresh"}),
        )
        .expect_err("missing file rejected");

    assert!(matches!(error, ToolError::Io(_)));
    assert!(!temp.path().join("missing.txt").exists());
}

#[test]
fn write_file_creates_missing_file_with_add_patch_events() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());

    let execution = registry
        .execute(
            "write_file",
            &json!({"path": "created.txt", "content": "fresh\n"}),
        )
        .expect("create patch");

    assert_eq!(execution.output, "created created.txt");
    let patch = execution.patch.expect("patch event");
    assert_eq!(patch.origin, "write_file");
    assert_eq!(patch.action, "add");
    assert_eq!(patch.before, "");
    assert_eq!(patch.after, "fresh\n");
    assert_eq!(patch.before_sha256, None);
    assert_eq!(patch.before_byte_len, 0);
    assert_eq!(patch.after_byte_len, 6);
    registry.apply_patch(&patch).expect("apply patch");
    assert_eq!(
        fs::read_to_string(temp.path().join("created.txt")).expect("created file"),
        "fresh\n"
    );
}

#[test]
fn write_file_rejects_existing_file_without_writing() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "existing").expect("fixture");
    let registry = ToolRegistry::new(temp.path());

    let error = registry
        .execute(
            "write_file",
            &json!({"path": "note.txt", "content": "clobber"}),
        )
        .expect_err("existing file rejected");

    assert!(matches!(error, ToolError::FileAlreadyExists));
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("read fixture"),
        "existing"
    );
}

#[test]
fn write_file_rejects_missing_parent_directory() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());

    let error = registry
        .execute(
            "write_file",
            &json!({"path": "missing/created.txt", "content": "fresh"}),
        )
        .expect_err("missing parent rejected");

    assert!(matches!(error, ToolError::ParentDirectoryMissing));
    assert!(!temp.path().join("missing").exists());
}

#[test]
fn write_file_rejects_absolute_and_traversal_paths_without_writing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());
    let absolute = std::env::temp_dir()
        .join("anything.txt")
        .to_string_lossy()
        .into_owned();

    let absolute_error = registry
        .execute("write_file", &json!({"path": absolute, "content": "x"}))
        .expect_err("absolute create rejected");
    let traversal_error = registry
        .execute(
            "write_file",
            &json!({"path": "../escape.txt", "content": "x"}),
        )
        .expect_err("traversal create rejected");

    assert!(matches!(
        absolute_error,
        ToolError::PathOutsideWorkspace {
            reason: "absolute paths are not allowed",
            ..
        }
    ));
    assert!(matches!(
        traversal_error,
        ToolError::PathOutsideWorkspace {
            reason: "path escapes the workspace root",
            ..
        }
    ));
    assert!(!temp
        .path()
        .parent()
        .expect("tempdir parent")
        .join("escape.txt")
        .exists());
}

#[cfg(unix)]
#[test]
fn write_file_rejects_create_through_symlink_escape() {
    let temp = tempfile::tempdir().expect("temp dir");
    let outside = tempfile::tempdir().expect("outside dir");
    symlink(outside.path(), temp.path().join("outside")).expect("symlink");
    let registry = ToolRegistry::new(temp.path());

    let error = registry
        .execute(
            "write_file",
            &json!({"path": "outside/created.txt", "content": "fresh"}),
        )
        .expect_err("escape rejected");

    assert!(matches!(
        error,
        ToolError::PathOutsideWorkspace {
            reason: "path escapes the workspace root",
            ..
        }
    ));
    assert!(!outside.path().join("created.txt").exists());
}

#[test]
fn write_file_requires_fs_write_capability() {
    let registry = ToolRegistry::new(".");

    assert_eq!(
        registry.required_capability("write_file"),
        Some(Capability::FsWrite)
    );
}

#[test]
fn model_tools_includes_write_file_with_create_only_semantics() {
    let registry = ToolRegistry::new(".");
    let write_file = registry
        .model_tools()
        .into_iter()
        .find(|tool| tool.name == "write_file")
        .expect("write_file schema");

    assert!(write_file.description.contains("Create a new"));
    assert!(write_file
        .description
        .contains("Fails if the file already exists"));
    assert_eq!(
        write_file.parameters["required"],
        json!(["path", "content"])
    );
    assert_eq!(write_file.parameters["additionalProperties"], json!(false));
}

#[test]
fn apply_patch_add_file_creates_missing_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());
    let patch = "*** Begin Patch\n*** Add File: created.txt\n+fresh\n*** End Patch";

    let execution = registry
        .execute("apply_patch", &json!({"patch": patch}))
        .expect("apply_patch add");

    let patch = execution.patch.expect("patch event");
    assert_eq!(patch.origin, "apply_patch");
    assert_eq!(patch.action, "add");
    assert_eq!(patch.after_byte_len, 6);
    registry.apply_patch(&patch).expect("write patch");
    assert_eq!(
        fs::read_to_string(temp.path().join("created.txt")).expect("created file"),
        "fresh\n"
    );
}

#[test]
fn apply_patch_update_uses_exact_hunk_and_hashes_whole_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let before = "prefix\nold\nsuffix\n";
    let after = "prefix\nnew\nsuffix\n";
    fs::write(temp.path().join("note.txt"), before).expect("fixture");
    let registry = ToolRegistry::new(temp.path());
    let patch = "*** Begin Patch\n*** Update File: note.txt\n@@\n prefix\n-old\n+new\n suffix\n*** End Patch";

    let execution = registry
        .execute("apply_patch", &json!({"patch": patch}))
        .expect("apply_patch update");

    let patch = execution.patch.expect("patch event");
    assert_eq!(patch.origin, "apply_patch");
    assert_eq!(patch.action, "modify");
    assert_eq!(patch.before_sha256, Some(hash_bytes(before.as_bytes())));
    assert_eq!(patch.after_sha256, hash_bytes(after.as_bytes()));
    assert_eq!(patch.before_byte_len, before.len());
    assert_eq!(patch.after_byte_len, after.len());
    registry.apply_patch(&patch).expect("write patch");
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("edited file"),
        after
    );
}

#[test]
fn apply_patch_update_accepts_multiple_hunks_for_one_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let before = "alpha\nold one\nmiddle\nold two\nomega\n";
    let after = "alpha\nnew one\nmiddle\nnew two\nomega\n";
    fs::write(temp.path().join("note.txt"), before).expect("fixture");
    let registry = ToolRegistry::new(temp.path());
    let patch = "*** Begin Patch\n*** Update File: note.txt\n@@\n alpha\n-old one\n+new one\n@@\n-old two\n+new two\n omega\n*** End Patch";

    let execution = registry
        .execute("apply_patch", &json!({"patch": patch}))
        .expect("apply_patch multi-hunk update");

    let patch = execution.patch.expect("patch event");
    assert_eq!(patch.origin, "apply_patch");
    assert_eq!(patch.action, "modify");
    assert_eq!(patch.before_sha256, Some(hash_bytes(before.as_bytes())));
    assert_eq!(patch.after_sha256, hash_bytes(after.as_bytes()));
    assert_eq!(patch.before_byte_len, before.len());
    assert_eq!(patch.after_byte_len, after.len());
    // Full file contents so diff projections can derive real line numbers.
    assert_eq!(patch.before, before);
    assert_eq!(patch.after, after);

    registry.apply_patch(&patch).expect("write patch");
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("edited file"),
        after
    );
}

#[test]
fn apply_patch_multi_hunk_failure_is_atomic_and_names_hunk() {
    let temp = tempfile::tempdir().expect("temp dir");
    let before = "alpha\nold one\nmiddle\nold two\nomega\n";
    fs::write(temp.path().join("note.txt"), before).expect("fixture");
    let registry = ToolRegistry::new(temp.path());
    let patch = "*** Begin Patch\n*** Update File: note.txt\n@@\n alpha\n-old one\n+new one\n middle\n@@\n middle\n-missing two\n+new two\n omega\n*** End Patch";

    let error = registry
        .execute("apply_patch", &json!({"patch": patch}))
        .expect_err("second hunk should fail");

    assert!(matches!(
        error,
        ToolError::UpdateHunkMatchCount { hunk: 2, count: 0 }
    ));
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("unchanged file"),
        before
    );
}

#[test]
fn apply_patch_multi_hunk_targets_original_file_content() {
    let temp = tempfile::tempdir().expect("temp dir");
    let before = "alpha\nomega\n";
    fs::write(temp.path().join("note.txt"), before).expect("fixture");
    let registry = ToolRegistry::new(temp.path());
    let patch = "*** Begin Patch\n*** Update File: note.txt\n@@\n-alpha\n+created\n@@\n-created\n+second\n*** End Patch";

    let error = registry
        .execute("apply_patch", &json!({"patch": patch}))
        .expect_err("later hunk must not match text created by an earlier hunk");

    assert!(matches!(
        error,
        ToolError::UpdateHunkMatchCount { hunk: 2, count: 0 }
    ));
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("unchanged file"),
        before
    );
}

#[test]
fn apply_patch_multi_hunk_rejects_overlapping_original_ranges() {
    let temp = tempfile::tempdir().expect("temp dir");
    let before = "alpha\nmiddle\nomega\n";
    fs::write(temp.path().join("note.txt"), before).expect("fixture");
    let registry = ToolRegistry::new(temp.path());
    let patch = "*** Begin Patch\n*** Update File: note.txt\n@@\n-alpha\n-middle\n+first\n@@\n-middle\n-omega\n+second\n*** End Patch";

    let error = registry
        .execute("apply_patch", &json!({"patch": patch}))
        .expect_err("overlapping hunks must be rejected");

    assert!(matches!(
        error,
        ToolError::UpdateHunkOverlap {
            hunk: 2,
            previous_hunk: 1
        }
    ));
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("unchanged file"),
        before
    );
}

#[test]
fn apply_patch_malformed_later_hunk_fails_without_writing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let before = "alpha\nold\nomega\n";
    fs::write(temp.path().join("note.txt"), before).expect("fixture");
    let registry = ToolRegistry::new(temp.path());
    let patch = "*** Begin Patch\n*** Update File: note.txt\n@@\n-old\n+new\n@@\n*** End Patch";

    let error = registry
        .execute("apply_patch", &json!({"patch": patch}))
        .expect_err("empty second hunk should fail");

    assert!(matches!(
        error,
        ToolError::InvalidPatch(message)
            if message.contains("every `@@` hunk must change something")
    ));
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("unchanged file"),
        before
    );
}

#[test]
fn apply_patch_update_crlf_file_requires_exact_bytes_and_writes_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let before = "prefix\r\nold\r\nsuffix\r\n";
    fs::write(temp.path().join("note.txt"), before).expect("fixture");
    let registry = ToolRegistry::new(temp.path());
    let patch = "*** Begin Patch\n*** Update File: note.txt\n@@\n prefix\n-old\n+new\n suffix\n*** End Patch";

    let error = registry
        .execute("apply_patch", &json!({"patch": patch}))
        .expect_err("LF patch must not silently normalize CRLF file");

    assert!(matches!(
        error,
        ToolError::UpdateHunkMatchCount { hunk: 1, count: 0 }
    ));
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("unchanged file"),
        before
    );
}

#[test]
fn apply_patch_update_non_utf8_file_fails_without_writing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let before = b"prefix\nold\nsuffix\xff\n";
    fs::write(temp.path().join("note.txt"), before).expect("fixture");
    let registry = ToolRegistry::new(temp.path());
    let patch = "*** Begin Patch\n*** Update File: note.txt\n@@\n-old\n+new\n*** End Patch";

    let error = registry
        .execute("apply_patch", &json!({"patch": patch}))
        .expect_err("non-UTF-8 patch target rejected");

    assert!(matches!(error, ToolError::Io(_)));
    assert_eq!(
        fs::read(temp.path().join("note.txt")).expect("unchanged bytes"),
        before
    );
}

#[test]
fn apply_patch_rejects_unsupported_actions_without_writing() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("obsolete.txt"), "keep").expect("fixture");
    let registry = ToolRegistry::new(temp.path());
    let delete_patch = "*** Begin Patch\n*** Delete File: obsolete.txt\n*** End Patch";
    let move_patch = "*** Begin Patch\n*** Update File: obsolete.txt\n*** Move to: moved.txt\n@@\n-keep\n+changed\n*** End Patch";

    let delete_error = registry
        .execute("apply_patch", &json!({"patch": delete_patch}))
        .expect_err("delete rejected");
    let move_error = registry
        .execute("apply_patch", &json!({"patch": move_patch}))
        .expect_err("move rejected");

    assert!(matches!(delete_error, ToolError::InvalidPatch(_)));
    assert!(matches!(move_error, ToolError::InvalidPatch(_)));
    assert_eq!(
        fs::read_to_string(temp.path().join("obsolete.txt")).expect("unchanged file"),
        "keep"
    );
    assert!(!temp.path().join("moved.txt").exists());
}

#[test]
fn apply_patch_rejects_multi_file_and_bad_paths_without_writing() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "old\n").expect("fixture");
    let registry = ToolRegistry::new(temp.path());
    let multi_file = "*** Begin Patch\n*** Add File: one.txt\n+one\n*** End Patch\n*** Add File: two.txt\n+two\n*** End Patch";
    let traversal = "*** Begin Patch\n*** Add File: ../escape.txt\n+nope\n*** End Patch";
    let absolute = "*** Begin Patch\n*** Add File: /tmp/escape.txt\n+nope\n*** End Patch";

    for patch in [multi_file, traversal, absolute] {
        assert!(
            registry
                .execute("apply_patch", &json!({"patch": patch}))
                .is_err(),
            "patch should fail: {patch}"
        );
    }

    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("unchanged file"),
        "old\n"
    );
    assert!(!temp.path().join("one.txt").exists());
    assert!(!temp.path().join("two.txt").exists());
}

#[test]
fn apply_patch_rejects_add_existing_and_update_missing_without_writing() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("existing.txt"), "keep\n").expect("fixture");
    let registry = ToolRegistry::new(temp.path());
    let add_existing = "*** Begin Patch\n*** Add File: existing.txt\n+replace\n*** End Patch";
    let update_missing =
        "*** Begin Patch\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch";

    let add_error = registry
        .execute("apply_patch", &json!({"patch": add_existing}))
        .expect_err("existing add rejected");
    let update_error = registry
        .execute("apply_patch", &json!({"patch": update_missing}))
        .expect_err("missing update rejected");

    assert!(matches!(add_error, ToolError::FileAlreadyExists));
    assert!(matches!(update_error, ToolError::Io(_)));
    assert_eq!(
        fs::read_to_string(temp.path().join("existing.txt")).expect("unchanged file"),
        "keep\n"
    );
    assert!(!temp.path().join("missing.txt").exists());
}

#[test]
fn apply_patch_rejects_zero_and_multiple_update_matches_without_writing() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("zero.txt"), "alpha\n").expect("zero fixture");
    fs::write(temp.path().join("many.txt"), "same\nsame\n").expect("many fixture");
    let registry = ToolRegistry::new(temp.path());
    let zero_match = "*** Begin Patch\n*** Update File: zero.txt\n@@\n-beta\n+new\n*** End Patch";
    let many_matches = "*** Begin Patch\n*** Update File: many.txt\n@@\n-same\n+new\n*** End Patch";

    let zero_error = registry
        .execute("apply_patch", &json!({"patch": zero_match}))
        .expect_err("zero matches rejected");
    let many_error = registry
        .execute("apply_patch", &json!({"patch": many_matches}))
        .expect_err("multiple matches rejected");

    assert!(matches!(
        zero_error,
        ToolError::UpdateHunkMatchCount { hunk: 1, count: 0 }
    ));
    assert!(matches!(
        many_error,
        ToolError::UpdateHunkMatchCount { hunk: 1, count: 2 }
    ));
    assert_eq!(
        fs::read_to_string(temp.path().join("zero.txt")).expect("unchanged zero"),
        "alpha\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("many.txt")).expect("unchanged many"),
        "same\nsame\n"
    );
}

#[test]
fn run_shell_strict_apply_patch_is_intercepted_without_shell_exec() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());
    let command = "apply_patch <<'PATCH'\n*** Begin Patch\n*** Add File: shell-created.txt\n+fresh\n*** End Patch\nPATCH";

    assert_eq!(
        registry.required_capability_for_input("run_shell", &json!({"command": command})),
        Some(Capability::FsWrite)
    );
    assert_eq!(
        registry.permission_reason("run_shell", &json!({"command": command})),
        "tool apply_patch"
    );

    let execution = registry
        .execute("run_shell", &json!({"command": command}))
        .expect("intercepted shell apply_patch");
    assert_eq!(execution.name, "run_shell");
    assert!(execution.output.contains("intercepted apply_patch"));

    let patch = execution.patch.expect("patch event");
    assert_eq!(patch.origin, "run_shell:apply_patch");
    registry.apply_patch(&patch).expect("write patch");
    assert_eq!(
        fs::read_to_string(temp.path().join("shell-created.txt")).expect("created file"),
        "fresh\n"
    );
}

#[test]
fn run_shell_intercept_accepts_multi_hunk_apply_patch() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("note.txt"), "one\nold\nmiddle\nstale\n").expect("fixture");
    let registry = ToolRegistry::new(temp.path());
    let command = "apply_patch <<'PATCH'\n*** Begin Patch\n*** Update File: note.txt\n@@\n one\n-old\n+new\n@@\n-stale\n+fresh\n*** End Patch\nPATCH";

    let execution = registry
        .execute("run_shell", &json!({"command": command}))
        .expect("intercepted shell multi-hunk apply_patch");
    let patch = execution.patch.expect("patch event");

    assert_eq!(patch.origin, "run_shell:apply_patch");
    registry.apply_patch(&patch).expect("write patch");
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).expect("edited file"),
        "one\nnew\nmiddle\nfresh\n"
    );
}

#[test]
fn run_shell_apply_patch_with_extra_shell_syntax_does_not_execute_shell() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());
    let command = "apply_patch <<'PATCH'\n*** Begin Patch\n*** Add File: created.txt\n+fresh\n*** End Patch\nPATCH\n; touch should-not-exist";

    let error = registry
        .execute("run_shell", &json!({"command": command}))
        .expect_err("mixed shell payload rejected");

    assert!(matches!(error, ToolError::InvalidPatch(_)));
    assert!(!temp.path().join("created.txt").exists());
    assert!(!temp.path().join("should-not-exist").exists());
}

#[test]
fn run_shell_apply_patch_token_without_strict_heredoc_does_not_execute_shell() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());

    for command in [
        "apply_patch --help; touch should-not-exist",
        "apply_patch <<TAG\n*** Begin Patch\n*** Add File: created.txt\n+fresh\n*** End Patch\nTAG",
        "apply_patch<<TAG\n*** Begin Patch\n*** Add File: created.txt\n+fresh\n*** End Patch\nTAG",
    ] {
        let error = registry
            .execute("run_shell", &json!({"command": command}))
            .expect_err("malformed apply_patch command rejected");
        assert!(matches!(error, ToolError::InvalidPatch(_)));
    }

    assert!(!temp.path().join("created.txt").exists());
    assert!(!temp.path().join("should-not-exist").exists());
}

#[test]
fn run_shell_no_space_apply_patch_heredoc_is_intercepted() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());
    let command = "apply_patch<<'PATCH'\n*** Begin Patch\n*** Add File: created.txt\n+fresh\n*** End Patch\nPATCH";

    let execution = registry
        .execute("run_shell", &json!({"command": command}))
        .expect("intercepted shell apply_patch");

    let patch = execution.patch.expect("patch event");
    assert_eq!(patch.origin, "run_shell:apply_patch");
    registry.apply_patch(&patch).expect("write patch");
    assert_eq!(
        fs::read_to_string(temp.path().join("created.txt")).expect("created file"),
        "fresh\n"
    );
}

#[test]
fn run_shell_apply_patch_heredoc_tag_collision_fails_without_shell_exec() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());
    let command = "apply_patch <<'PATCH'\n*** Begin Patch\n*** Add File: created.txt\n+fresh\nPATCH\n+after-tag\n*** End Patch\nPATCH";

    let error = registry
        .execute("run_shell", &json!({"command": command}))
        .expect_err("tag collision rejected");

    assert!(matches!(error, ToolError::InvalidPatch(_)));
    assert!(!temp.path().join("created.txt").exists());
}

#[test]
fn run_shell_apply_patch_prefix_collision_executes_as_ordinary_shell() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());

    let execution = registry
        .execute(
            "run_shell",
            &json!({"command": "apply_patch_helper; printf ok > helper.txt"}),
        )
        .expect("ordinary shell");

    assert!(execution.patch.is_none());
    assert_eq!(
        fs::read_to_string(temp.path().join("helper.txt")).expect("helper file"),
        "ok"
    );
}

#[test]
fn non_apply_patch_shell_commands_still_execute_normally() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());

    let execution = registry
        .execute("run_shell", &json!({"command": "printf ok > shell.txt"}))
        .expect("ordinary shell");

    assert!(execution.patch.is_none());
    assert_eq!(execution.exit_code, Some(0));
    assert_eq!(
        fs::read_to_string(temp.path().join("shell.txt")).expect("shell file"),
        "ok"
    );
}

#[test]
fn run_shell_scrubs_secret_and_parent_control_env_and_keeps_ordinary_env() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let _env_restore = EnvRestore::capture(&[
        "EULER_ORDINARY_VAR",
        "ANTHROPIC_API_KEY",
        "EULER_AUTH_FILE",
        "EULER_CUSTOM_API_KEY",
        "AWS_SECRET_ACCESS_KEY",
        "EULER_TEST_TOKEN",
        "EULER_TEST_SECRET",
        "EULER_TOKENIZER_MODE",
        "EULER_HOME",
        "EULER_PROVIDER",
        "EULER_MODEL",
        "EULER_NO_TTY",
        "EULER_TUI_METRICS",
        "RUST_LOG",
    ]);
    env::set_var("EULER_ORDINARY_VAR", "visible");
    env::set_var("ANTHROPIC_API_KEY", "anthropic-secret");
    env::set_var("EULER_AUTH_FILE", "auth-file-secret");
    env::set_var("EULER_CUSTOM_API_KEY", "api-key-secret");
    env::set_var("AWS_SECRET_ACCESS_KEY", "access-key-secret");
    env::set_var("EULER_TEST_TOKEN", "token-secret");
    env::set_var("EULER_TEST_SECRET", "generic-secret");
    env::set_var("EULER_TOKENIZER_MODE", "tokenizer-visible");
    env::set_var("EULER_HOME", "/parent/euler-home");
    env::set_var("EULER_PROVIDER", "parent-provider");
    env::set_var("EULER_MODEL", "parent-model");
    env::set_var("EULER_NO_TTY", "1");
    env::set_var("EULER_TUI_METRICS", "/parent/metrics.jsonl");
    env::set_var("RUST_LOG", "trace");
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());

    let execution = registry
            .execute(
                "run_shell",
                &json!({
                    "command": "printf '%s|%s|%s|%s|%s|%s|%s|%s|%s|%s|%s|%s|%s|%s' \"$EULER_ORDINARY_VAR\" \"$ANTHROPIC_API_KEY\" \"$EULER_AUTH_FILE\" \"$EULER_CUSTOM_API_KEY\" \"$AWS_SECRET_ACCESS_KEY\" \"$EULER_TEST_TOKEN\" \"$EULER_TEST_SECRET\" \"$EULER_TOKENIZER_MODE\" \"$EULER_HOME\" \"$EULER_PROVIDER\" \"$EULER_MODEL\" \"$EULER_NO_TTY\" \"$EULER_TUI_METRICS\" \"$RUST_LOG\""
                }),
            )
            .expect("shell");

    assert!(execution
        .output
        .contains("visible|||||||tokenizer-visible||||||"));
    assert!(!execution.output.contains("anthropic-secret"));
    assert!(!execution.output.contains("auth-file-secret"));
    assert!(!execution.output.contains("api-key-secret"));
    assert!(!execution.output.contains("access-key-secret"));
    assert!(!execution.output.contains("token-secret"));
    assert!(!execution.output.contains("generic-secret"));

    let explicit = registry
        .execute(
            "run_shell",
            &json!({
                "command": "EULER_HOME=/explicit; RUST_LOG=debug; printf '%s|%s' \"$EULER_HOME\" \"$RUST_LOG\""
            }),
        )
        .expect("shell with explicit controls");
    assert!(explicit.output.contains("/explicit|debug"));
}

#[test]
fn run_shell_rejects_zero_max_bytes() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());

    let error = registry
        .execute(
            "run_shell",
            &json!({"command": "printf content", "max_bytes": 0}),
        )
        .expect_err("zero max_bytes rejected");

    assert!(matches!(error, ToolError::InvalidField("max_bytes")));
}

#[cfg(unix)]
#[test]
fn resolve_path_rejects_symlink_escape() {
    let temp = tempfile::tempdir().expect("temp dir");
    let outside = tempfile::tempdir().expect("outside dir");
    fs::write(outside.path().join("secret.txt"), "secret").expect("outside file");
    symlink(
        outside.path().join("secret.txt"),
        temp.path().join("link.txt"),
    )
    .expect("symlink");
    let registry = ToolRegistry::new(temp.path());

    let error = registry
        .execute("read_file", &json!({"path": "link.txt"}))
        .expect_err("escape rejected");

    assert!(matches!(
        error,
        ToolError::PathOutsideWorkspace {
            reason: "path escapes the workspace root",
            ..
        }
    ));
}

#[test]
fn apply_patch_rejects_absolute_add_path_with_actionable_diagnostic() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());
    let patch =
        "*** Begin Patch\n*** Add File: /tmp/optimize_complex_function.py\n+print()\n*** End Patch";

    let error = registry
        .execute("apply_patch", &json!({"patch": patch}))
        .expect_err("absolute path rejected");

    assert!(matches!(
        error,
        ToolError::PathOutsideWorkspace {
            reason: "absolute paths are not allowed",
            ..
        }
    ));
    let message = error.to_string();
    assert!(
        message.contains("/tmp/optimize_complex_function.py"),
        "message names the path: {message}"
    );
    assert!(
        message.contains("outside the workspace root"),
        "message states the real cause: {message}"
    );
    assert!(
        message.contains("paths must be relative and stay inside the workspace root"),
        "message says how to proceed: {message}"
    );
    assert!(
        !message.contains("invalid field"),
        "message must not look like a schema error: {message}"
    );
}

#[test]
fn apply_patch_rejects_traversal_add_path_with_actionable_diagnostic() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());
    let patch = "*** Begin Patch\n*** Add File: ../escape.py\n+print()\n*** End Patch";

    let error = registry
        .execute("apply_patch", &json!({"patch": patch}))
        .expect_err("traversal path rejected");

    assert!(matches!(error, ToolError::PathOutsideWorkspace { .. }));
    let message = error.to_string();
    assert!(
        message.contains("../escape.py"),
        "message names the path: {message}"
    );
    assert!(
        message.contains("outside the workspace root"),
        "message states the real cause: {message}"
    );
}

#[test]
fn read_file_and_edit_file_reject_absolute_paths_with_actionable_diagnostic() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());
    let absolute = std::env::temp_dir()
        .join("anything.txt")
        .to_string_lossy()
        .into_owned();

    let read_error = registry
        .execute("read_file", &json!({"path": absolute}))
        .expect_err("absolute read rejected");
    let edit_error = registry
        .execute(
            "edit_file",
            &json!({"path": absolute, "old": "", "new": "content"}),
        )
        .expect_err("absolute create rejected");

    for error in [read_error, edit_error] {
        assert!(matches!(
            error,
            ToolError::PathOutsideWorkspace {
                reason: "absolute paths are not allowed",
                ..
            }
        ));
        let message = error.to_string();
        assert!(message.contains("anything.txt"));
        assert!(message.contains("outside the workspace root"));
    }
}

#[test]
fn empty_path_reports_invalid_field() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());

    let read_error = registry
        .execute("read_file", &json!({"path": ""}))
        .expect_err("empty read path rejected");
    let edit_error = registry
        .execute("edit_file", &json!({"path": "", "old": "", "new": "x"}))
        .expect_err("empty create path rejected");

    assert!(matches!(read_error, ToolError::InvalidField("path")));
    assert!(matches!(edit_error, ToolError::InvalidField("path")));
}

#[test]
fn path_diagnostic_sanitizes_control_chars_and_truncates_long_paths() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());

    let hostile = "/tmp/\u{1b}[31mred\nline.txt";
    let error = registry
        .execute("read_file", &json!({"path": hostile}))
        .expect_err("hostile absolute path rejected");
    let message = error.to_string();
    assert!(!message.contains('\u{1b}'), "escape stripped: {message:?}");
    assert!(!message.contains('\n'), "newline stripped: {message:?}");
    assert!(message.contains("red"));

    let long = format!("/{}", "a".repeat(1000));
    let error = registry
        .execute("read_file", &json!({"path": long}))
        .expect_err("long absolute path rejected");
    let message = error.to_string();
    assert!(
        message.chars().count() < 400,
        "message bounded: {} chars",
        message.chars().count()
    );
    assert!(message.contains('\u{2026}'), "truncation marked");
}

#[test]
fn non_path_schema_errors_still_report_invalid_field() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());

    let error = registry
        .execute("read_file", &json!({"path": 42}))
        .expect_err("non-string path rejected");

    assert!(matches!(error, ToolError::InvalidField("path")));
}

#[test]
fn run_shell_kills_command_and_process_group_at_timeout() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());
    let started = std::time::Instant::now();
    let execution = registry
        .execute(
            "run_shell",
            &json!({
                "command": "echo phase_one; sleep 30 & sleep 30; echo phase_two",
                "timeout_ms": 200
            }),
        )
        .expect("timeout is a tool result, not an error");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(execution.exit_code, Some(-1));
    assert!(execution.output.contains("timed out after 200 ms"));
    assert!(execution.output.contains("phase_one"));
    assert!(!execution.output.contains("phase_two"));
}

#[test]
fn run_shell_timeout_ms_is_bounded() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());
    let error = registry
        .execute(
            "run_shell",
            &json!({"command": "true", "timeout_ms": 600_001}),
        )
        .expect_err("over-cap timeout rejected");
    assert!(error.to_string().contains("timeout_ms"));
}

#[test]
fn run_shell_fast_command_unaffected_by_timeout_default() {
    let temp = tempfile::tempdir().expect("temp dir");
    let registry = ToolRegistry::new(temp.path());
    let execution = registry
        .execute("run_shell", &json!({"command": "printf fast"}))
        .expect("fast command");
    assert_eq!(execution.exit_code, Some(0));
    assert!(execution.output.contains("fast"));
    assert!(!execution.output.contains("timed out"));
}

#[test]
fn tool_result_get_rehydrates_session_tool_result_by_event_id() {
    use euler_event::{object, EventEnvelope, EventKind};
    let event = EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::TOOL_RESULT,
        object([
            ("id", "call-1".into()),
            ("name", "read_file".into()),
            ("ok", true.into()),
            ("output", "full file body\nline 2".into()),
        ]),
    );
    let event_id = event.id.clone();
    let registry = ToolRegistry::new(".");
    let execution = registry
        .execute_with_events("tool_result_get", &json!({"event_id": event_id}), &[event])
        .expect("rehydrate");
    assert!(execution.output.contains("full file body"));
    assert!(execution.output.contains("rehydrated read_file"));
}

#[test]
fn model_tools_includes_tool_result_get() {
    let registry = ToolRegistry::new(".");
    assert!(registry
        .model_tools()
        .iter()
        .any(|tool| tool.name == "tool_result_get"));
}

// --- rung-2 re-teach escalation (issue #94) -------------------------------

/// Every `*** Begin Patch` block in the re-teach payload must parse with
/// the real parser: the payload teaches the format, so it can never drift
/// into syntax the parser rejects.
#[test]
fn reteach_examples_parse() {
    let mut blocks = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in APPLY_PATCH_RETEACH.lines() {
        if line == "*** Begin Patch" {
            current = Some(vec![line]);
        } else if let Some(block) = current.as_mut() {
            block.push(line);
            if line == "*** End Patch" {
                blocks.push(current.take().expect("open block").join("\n"));
            }
        }
    }
    assert_eq!(blocks.len(), 2, "payload shows one Add and one Update");
    let add = parse_single_file_apply_patch(&blocks[0]).expect("add example parses");
    assert!(matches!(add, ApplyPatchDocument::Add { .. }));
    let update = parse_single_file_apply_patch(&blocks[1]).expect("update example parses");
    let ApplyPatchDocument::Update { chunks, .. } = update else {
        panic!("second example must be an update");
    };
    assert_eq!(chunks.len(), 1);
}

#[test]
fn second_consecutive_failure_appends_full_payload_first_does_not() {
    let registry = ToolRegistry::new(".");
    let mut tracker = ReteachTracker::default();
    let input = json!({"patch": "nope"});
    let first = registry.teach_on_failure(&mut tracker, "apply_patch", &input, "bad".to_owned());
    assert_eq!(first, "bad", "first failure keeps the rung-1 one-liner");
    let second = registry.teach_on_failure(&mut tracker, "apply_patch", &input, "bad".to_owned());
    assert_eq!(second, format!("bad\n\n{APPLY_PATCH_RETEACH}"));
    let third = registry.teach_on_failure(&mut tracker, "apply_patch", &input, "bad".to_owned());
    assert_eq!(
        third,
        format!("bad\n\n{APPLY_PATCH_RETEACH}"),
        "the streak keeps teaching until a success resets it"
    );
}

#[test]
fn success_resets_streak_to_the_one_liner() {
    let registry = ToolRegistry::new(".");
    let mut tracker = ReteachTracker::default();
    let input = json!({"patch": "nope"});
    let _ = registry.teach_on_failure(&mut tracker, "apply_patch", &input, "bad".to_owned());
    tracker.record_success(registry.reteach_identity("apply_patch", &input));
    let after_success =
        registry.teach_on_failure(&mut tracker, "apply_patch", &input, "bad".to_owned());
    assert_eq!(
        after_success, "bad",
        "failure -> success -> failure is a fresh streak"
    );
}

#[test]
fn streaks_are_per_tool_and_other_tools_never_reset_them() {
    let registry = ToolRegistry::new(".");
    let mut tracker = ReteachTracker::default();
    let patch_input = json!({"patch": "nope"});
    let edit_input = json!({"path": "x"});
    let _ = registry.teach_on_failure(&mut tracker, "apply_patch", &patch_input, "a1".to_owned());
    // Another tool fails and a third tool succeeds in between; neither
    // touches apply_patch's streak.
    let b_error =
        registry.teach_on_failure(&mut tracker, "edit_file", &edit_input, "b1".to_owned());
    let _ = registry.teach_on_failure(&mut tracker, "edit_file", &edit_input, "b2".to_owned());
    tracker.record_success("read_file");
    let a_second =
        registry.teach_on_failure(&mut tracker, "apply_patch", &patch_input, "a2".to_owned());
    assert_eq!(b_error, "b1", "tools without a payload never escalate");
    assert_eq!(a_second, format!("a2\n\n{APPLY_PATCH_RETEACH}"));
}

#[test]
fn escalation_is_deterministic_across_fresh_trackers() {
    let registry = ToolRegistry::new(".");
    let input = json!({"patch": "nope"});
    let run = || {
        let mut tracker = ReteachTracker::default();
        (0..3)
            .map(|_| {
                registry.teach_on_failure(&mut tracker, "apply_patch", &input, "bad".to_owned())
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(run(), run(), "same failure sequence, same strings");
}

#[test]
fn intercepted_run_shell_heredoc_counts_against_apply_patch() {
    let registry = ToolRegistry::new(".");
    let mut tracker = ReteachTracker::default();
    let heredoc = json!({"command": "apply_patch <<'EOF'\nnot a patch\nEOF"});
    assert_eq!(
        registry.reteach_identity("run_shell", &heredoc),
        "apply_patch"
    );
    assert_eq!(
        registry.reteach_identity("run_shell", &json!({"command": "ls"})),
        "run_shell"
    );
    let _ = registry.teach_on_failure(&mut tracker, "apply_patch", &json!({}), "a1".to_owned());
    let second = registry.teach_on_failure(&mut tracker, "run_shell", &heredoc, "a2".to_owned());
    assert_eq!(
        second,
        format!("a2\n\n{APPLY_PATCH_RETEACH}"),
        "a failed apply_patch heredoc through run_shell continues the apply_patch streak"
    );
}

#[test]
fn selected_sandbox_normalizes_subprocess_io_failures() {
    let sandboxed = normalize_sandbox_subprocess_error(
        true,
        ToolError::Io(std::io::Error::other("raw launcher detail")),
    );
    assert!(matches!(
        sandboxed,
        ToolError::SandboxUnavailable(SandboxUnavailableReason::CannotEnforce)
    ));

    let host = normalize_sandbox_subprocess_error(
        false,
        ToolError::Io(std::io::Error::other("ordinary host error")),
    );
    assert!(matches!(host, ToolError::Io(_)));
}

#[test]
fn sandbox_timeout_before_readiness_hides_launcher_output() {
    let output = collected_agent_output(
        "bwrap: host mount detail".to_owned(),
        "more host detail".to_owned(),
        true,
        true,
    )
    .expect("timeout is not a sandbox availability failure");

    assert!(output.is_empty());
}
