use super::{
    apply_patch_update_chunks, parse_single_file_apply_patch, ApplyPatchDocument, ApplyPatchError,
};

#[test]
fn parses_multi_hunk_update() {
    let patch = "*** Begin Patch\n*** Update File: note.txt\n@@\n-old\n+new\n@@\n-stale\n+fresh\n*** End Patch";

    let parsed = parse_single_file_apply_patch(patch).expect("parse patch");

    let ApplyPatchDocument::Update { path, chunks } = parsed else {
        panic!("expected update");
    };
    assert_eq!(path, "note.txt");
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].old, "old\n");
    assert_eq!(chunks[0].new, "new\n");
    assert_eq!(chunks[1].old, "stale\n");
    assert_eq!(chunks[1].new, "fresh\n");
}

#[test]
fn applies_non_overlapping_hunks_against_original_content() {
    let ApplyPatchDocument::Update { chunks, .. } = parse_single_file_apply_patch(
        "*** Begin Patch\n*** Update File: note.txt\n@@\n-alpha\n+created\n@@\n-omega\n+done\n*** End Patch",
    )
    .expect("parse patch") else {
        panic!("expected update");
    };

    let updated = apply_patch_update_chunks("alpha\nmiddle\nomega\n", &chunks).expect("apply");

    assert_eq!(updated, "created\nmiddle\ndone\n");
}

#[test]
fn applies_replacements_from_end_to_start_after_original_offsets() {
    let ApplyPatchDocument::Update { chunks, .. } = parse_single_file_apply_patch(
        "*** Begin Patch\n*** Update File: note.txt\n@@\n-alpha\n+alpha-expanded\n@@\n-omega\n+omega-expanded\n*** End Patch",
    )
    .expect("parse patch") else {
        panic!("expected update");
    };

    let updated = apply_patch_update_chunks("alpha\nmiddle\nomega\n", &chunks).expect("apply");

    assert_eq!(updated, "alpha-expanded\nmiddle\nomega-expanded\n");
}

#[test]
fn later_hunks_do_not_match_text_created_by_earlier_hunks() {
    let ApplyPatchDocument::Update { chunks, .. } = parse_single_file_apply_patch(
        "*** Begin Patch\n*** Update File: note.txt\n@@\n-alpha\n+created\n@@\n-created\n+second\n*** End Patch",
    )
    .expect("parse patch") else {
        panic!("expected update");
    };

    let error = apply_patch_update_chunks("alpha\nomega\n", &chunks).expect_err("hunk fails");

    assert_eq!(
        error,
        ApplyPatchError::UpdateHunkMatchCount { hunk: 2, count: 0 }
    );
}

#[test]
fn overlapping_hunks_are_rejected() {
    let ApplyPatchDocument::Update { chunks, .. } = parse_single_file_apply_patch(
        "*** Begin Patch\n*** Update File: note.txt\n@@\n-alpha\n-middle\n+first\n@@\n-middle\n-omega\n+second\n*** End Patch",
    )
    .expect("parse patch") else {
        panic!("expected update");
    };

    let error = apply_patch_update_chunks("alpha\nmiddle\nomega\n", &chunks).expect_err("overlap");

    assert_eq!(
        error,
        ApplyPatchError::UpdateHunkOverlap {
            hunk: 2,
            previous_hunk: 1
        }
    );
}
