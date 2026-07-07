use euler_core::{
    capture_workspace_snapshot, observed_file_diff_projection, MAX_WORKSPACE_SNAPSHOT_FILES,
    MAX_WORKSPACE_SNAPSHOT_FILE_BYTES,
};
use std::fs;

#[test]
fn workspace_snapshot_skips_local_state_directories() {
    let temp = tempfile::tempdir().expect("temp dir");
    let before = capture_workspace_snapshot(temp.path()).expect("before snapshot");
    fs::create_dir(temp.path().join(".euler")).expect("create state dir");
    fs::write(temp.path().join(".euler/session.jsonl"), "secret=hidden\n").expect("write state");
    let after = capture_workspace_snapshot(temp.path()).expect("after snapshot");

    assert!(before.changes_to(&after).is_empty());
}

#[test]
fn oversized_added_file_emits_metadata_only_diff() {
    let temp = tempfile::tempdir().expect("temp dir");
    let before = capture_workspace_snapshot(temp.path()).expect("before snapshot");
    fs::write(
        temp.path().join("large.txt"),
        vec![b'x'; MAX_WORKSPACE_SNAPSHOT_FILE_BYTES + 1],
    )
    .expect("write large file");
    let after = capture_workspace_snapshot(temp.path()).expect("after snapshot");

    let changes = before.changes_to(&after);
    assert_eq!(changes.len(), 1);
    let change = &changes[0];
    assert_eq!(change.path, "large.txt");
    assert_eq!(change.action, "add");
    assert_eq!(change.after_sha256, None);
    assert_eq!(change.after_byte_len, MAX_WORKSPACE_SNAPSHOT_FILE_BYTES + 1);

    let projection = observed_file_diff_projection(change);
    assert_eq!(projection.diff, None);
    assert_eq!(
        projection.omitted_reason.as_deref(),
        Some("content-unobserved")
    );
}

#[test]
fn file_count_cap_failure_emits_no_partial_changes() {
    let temp = tempfile::tempdir().expect("temp dir");
    let before = capture_workspace_snapshot(temp.path()).expect("before snapshot");
    for index in 0..=MAX_WORKSPACE_SNAPSHOT_FILES {
        fs::write(temp.path().join(format!("{index:04}.txt")), "x\n").expect("write file");
    }
    let after = capture_workspace_snapshot(temp.path()).expect("after snapshot");

    assert!(before.changes_to(&after).is_empty());
}
