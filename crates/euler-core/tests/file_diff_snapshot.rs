use euler_core::{
    capture_workspace_snapshot, observed_file_diff_projection,
    validate_workspace_snapshot_hardlinks, WorkspaceSnapshotError, MAX_WORKSPACE_SNAPSHOT_FILES,
    MAX_WORKSPACE_SNAPSHOT_FILE_BYTES,
};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{symlink, PermissionsExt as _};

fn change_at<'a>(
    changes: &'a [euler_core::ObservedFileChange],
    path: &str,
) -> &'a euler_core::ObservedFileChange {
    changes
        .iter()
        .find(|change| change.path == path)
        .unwrap_or_else(|| panic!("missing change for {path}: {changes:?}"))
}

#[test]
fn workspace_snapshot_reports_local_state_as_one_opaque_change() {
    let temp = tempfile::tempdir().expect("temp dir");
    let before = capture_workspace_snapshot(temp.path()).expect("before snapshot");
    fs::create_dir(temp.path().join(".euler")).expect("create state dir");
    fs::write(temp.path().join(".euler/session.jsonl"), "secret=hidden\n").expect("write state");
    let after = capture_workspace_snapshot(temp.path()).expect("after snapshot");

    let changes = before.changes_to(&after);
    let change = change_at(&changes, ".euler");
    assert_eq!(change.file_type, "opaque-directory");
    assert_eq!(change.action, "add");
    assert_eq!(
        observed_file_diff_projection(change)
            .omitted_reason
            .as_deref(),
        Some("excluded-surface")
    );
}

#[test]
fn same_size_content_mutation_inside_git_changes_opaque_digest() {
    let temp = tempfile::tempdir().expect("temp dir");
    let reference = temp.path().join(".git/refs/heads/main");
    fs::create_dir_all(reference.parent().expect("refs parent")).expect("create refs");
    fs::write(&reference, "aaaaaaaa\n").expect("initial ref");
    let before = capture_workspace_snapshot(temp.path()).expect("before snapshot");

    fs::write(&reference, "bbbbbbbb\n").expect("same-size ref mutation");
    let after = capture_workspace_snapshot(temp.path()).expect("after snapshot");

    let changes = before.changes_to(&after);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, ".git");
    assert_eq!(changes[0].file_type, "opaque-directory");
    assert_ne!(changes[0].before_sha256, changes[0].after_sha256);
    assert_eq!(
        observed_file_diff_projection(&changes[0])
            .omitted_reason
            .as_deref(),
        Some("excluded-surface")
    );
}

#[cfg(unix)]
#[test]
fn symlink_create_retarget_and_delete_are_observed() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("first"), "one").expect("first target");
    fs::write(temp.path().join("second"), "two").expect("second target");
    let before = capture_workspace_snapshot(temp.path()).expect("before snapshot");

    symlink("first", temp.path().join("current")).expect("create symlink");
    let created = capture_workspace_snapshot(temp.path()).expect("created snapshot");
    let added = before.changes_to(&created);
    let added = change_at(&added, "current");
    assert_eq!(added.file_type, "symlink");
    assert_eq!(added.action, "add");

    fs::remove_file(temp.path().join("current")).expect("remove old symlink");
    symlink("second", temp.path().join("current")).expect("retarget symlink");
    let retargeted = capture_workspace_snapshot(temp.path()).expect("retargeted snapshot");
    let modified = created.changes_to(&retargeted);
    let modified = change_at(&modified, "current");
    assert_eq!(modified.action, "modify");
    assert_ne!(modified.before_sha256, modified.after_sha256);

    fs::remove_file(temp.path().join("current")).expect("delete symlink");
    let deleted = capture_workspace_snapshot(temp.path()).expect("deleted snapshot");
    let removed = retargeted.changes_to(&deleted);
    let removed = change_at(&removed, "current");
    assert_eq!(removed.file_type, "symlink");
    assert_eq!(removed.action, "delete");
}

#[test]
fn empty_directory_create_and_delete_are_observed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let before = capture_workspace_snapshot(temp.path()).expect("before snapshot");
    fs::create_dir(temp.path().join("empty")).expect("create empty directory");
    let created = capture_workspace_snapshot(temp.path()).expect("created snapshot");

    let added = before.changes_to(&created);
    let added = change_at(&added, "empty");
    assert_eq!(added.file_type, "directory");
    assert_eq!(added.action, "add");

    fs::remove_dir(temp.path().join("empty")).expect("delete empty directory");
    let deleted = capture_workspace_snapshot(temp.path()).expect("deleted snapshot");
    let removed = created.changes_to(&deleted);
    let removed = change_at(&removed, "empty");
    assert_eq!(removed.action, "delete");
}

#[cfg(unix)]
#[test]
fn writable_root_chmod_is_observed_at_dot() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).expect("initial mode");
    let before = capture_workspace_snapshot(temp.path()).expect("before snapshot");

    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o750)).expect("changed mode");
    let after = capture_workspace_snapshot(temp.path()).expect("after snapshot");
    let changes = before.changes_to(&after);
    let root = change_at(&changes, ".");

    assert_eq!(root.file_type, "directory");
    assert_eq!(root.action, "modify");
    assert_eq!(root.before_mode, Some(0o700));
    assert_eq!(root.after_mode, Some(0o750));
    assert_eq!(
        observed_file_diff_projection(root)
            .omitted_reason
            .as_deref(),
        Some("metadata-only")
    );
}

#[cfg(unix)]
#[test]
fn chmod_is_a_metadata_only_change() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("script.sh");
    fs::write(&path, "#!/bin/sh\n").expect("write script");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("initial mode");
    let before = capture_workspace_snapshot(temp.path()).expect("before snapshot");

    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("executable mode");
    let after = capture_workspace_snapshot(temp.path()).expect("after snapshot");
    let changes = before.changes_to(&after);

    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "script.sh");
    assert_eq!(changes[0].before_sha256, changes[0].after_sha256);
    assert_eq!(changes[0].before_mode, Some(0o600));
    assert_eq!(changes[0].after_mode, Some(0o755));
    assert_ne!(
        changes[0].before_metadata_sha256,
        changes[0].after_metadata_sha256
    );
    assert_eq!(
        observed_file_diff_projection(&changes[0])
            .omitted_reason
            .as_deref(),
        Some("metadata-only")
    );
}

#[cfg(unix)]
#[test]
fn hardlink_alias_outside_writable_roots_is_rejected() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    fs::create_dir(&workspace).expect("workspace");
    fs::create_dir(&outside).expect("outside");
    let outside_file = outside.join("shared");
    fs::write(&outside_file, "same inode").expect("outside file");
    fs::hard_link(&outside_file, workspace.join("alias")).expect("hardlink into workspace");

    let snapshot = capture_workspace_snapshot(&workspace).expect("snapshot");
    assert_eq!(
        validate_workspace_snapshot_hardlinks(&[snapshot]),
        Err(WorkspaceSnapshotError::ExternalHardlink)
    );
}

#[cfg(unix)]
#[test]
fn hardlinks_fully_inside_writable_roots_are_complete() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(temp.path().join("first"), "same inode").expect("first link");
    fs::hard_link(temp.path().join("first"), temp.path().join("second")).expect("second link");

    let snapshot = capture_workspace_snapshot(temp.path()).expect("snapshot");
    validate_workspace_snapshot_hardlinks(&[snapshot]).expect("all aliases observed");
}

#[cfg(unix)]
#[test]
fn snapshot_rejects_a_symlink_substituted_writable_root() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("temp dir");
    let selected = temp.path().join("selected");
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).expect("outside");
    symlink(&outside, &selected).expect("root alias");

    assert_eq!(
        capture_workspace_snapshot(&selected),
        Err(WorkspaceSnapshotError::InvalidRoot)
    );
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
    let change = change_at(&changes, "large.txt");
    assert_eq!(change.action, "add");
    assert!(change.after_sha256.is_some());
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
    let error = capture_workspace_snapshot(temp.path()).expect_err("snapshot must fail closed");

    assert_eq!(error, WorkspaceSnapshotError::FileLimit);
    assert!(before.changes_to(&before).is_empty());
}

#[test]
fn oversized_same_length_mutation_is_detected_by_digest() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("large.bin");
    fs::write(&path, vec![b'a'; MAX_WORKSPACE_SNAPSHOT_FILE_BYTES + 1]).expect("initial file");
    let before = capture_workspace_snapshot(temp.path()).expect("before snapshot");
    fs::write(&path, vec![b'b'; MAX_WORKSPACE_SNAPSHOT_FILE_BYTES + 1]).expect("mutate file");
    let after = capture_workspace_snapshot(temp.path()).expect("after snapshot");

    let changes = before.changes_to(&after);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].action, "modify");
    assert_ne!(changes[0].before_sha256, changes[0].after_sha256);
    assert_eq!(
        observed_file_diff_projection(&changes[0])
            .omitted_reason
            .as_deref(),
        Some("content-unobserved")
    );
}
