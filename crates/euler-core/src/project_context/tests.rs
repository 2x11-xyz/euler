//! Substrate tests for discovery containment, bounds, digests, privacy,
//! folding, and workspace identity. Session-seam behavior (provider request
//! capture, child filtering, resume) is covered in `session_test.rs` and
//! `tests/resume.rs`-style integration tests.

use super::fold::{
    fold_project_context, validate_bootstrap_shape, verify_workspace_identity, ProjectContextFold,
    WorkspaceIdentityIssue,
};
use super::*;
use euler_event::{EventEnvelope, EventKind};
use std::fs;
use std::path::{Path, PathBuf};

fn redactor() -> SecretRedactor {
    SecretRedactor::new()
}

fn admitted(root: &Path) -> ProjectContextBootstrap {
    ProjectContextBootstrap::admitted_for_tests(root, &redactor()).expect("preflight")
}

fn dormant(root: &Path) -> ProjectContextBootstrap {
    ProjectContextBootstrap::dormant(root, &redactor()).expect("preflight")
}

fn write(path: &Path, content: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, content).expect("write");
}

fn git_dir(root: &Path) {
    fs::create_dir_all(root.join(".git")).expect("git dir");
}

fn source_paths(bootstrap: &ProjectContextBootstrap) -> Vec<String> {
    bootstrap.source_identities.clone()
}

fn reasons(bootstrap: &ProjectContextBootstrap) -> Vec<String> {
    bootstrap
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.reason.clone())
        .collect()
}

fn manifest_sources(bootstrap: &ProjectContextBootstrap) -> Vec<(String, String)> {
    bootstrap
        .manifest
        .as_ref()
        .expect("admitted manifest")
        .sources
        .iter()
        .map(|source| (source.path.clone(), source.content.clone()))
        .collect()
}

/// Build the durable bootstrap event sequence the session constructor
/// writes, without a session.
fn bootstrap_events(bootstrap: &ProjectContextBootstrap) -> Vec<EventEnvelope> {
    let mut start_payload =
        euler_event::object([("provider", "fixture".into()), ("model", "echo".into())]);
    start_payload.insert(
        "project_context".to_owned(),
        bootstrap.session_start_summary(),
    );
    let start = EventEnvelope::new(
        "session",
        "root",
        None,
        EventKind::SESSION_START,
        start_payload,
    );
    let snapshot = EventEnvelope::new(
        "session",
        "root",
        Some(start.id.clone()),
        EventKind::PROJECT_CONTEXT_SNAPSHOT,
        bootstrap.snapshot_payload(),
    );
    let mut events = vec![start, snapshot];
    let snapshot_id = events[1].id.clone();
    for payload in bootstrap.diagnostic_payloads(&snapshot_id) {
        events.push(EventEnvelope::new(
            "session",
            "root",
            Some(snapshot_id.clone()),
            EventKind::PROJECT_CONTEXT_DIAGNOSTIC,
            payload,
        ));
    }
    events
}

// ---------------------------------------------------------------------------
// Discovery: markers, boundaries, chain
// ---------------------------------------------------------------------------

#[test]
fn git_directory_marker_defines_the_chain_root_first() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    write(&repo.join("EULER.md"), "root rules");
    write(&repo.join("crates").join("EULER.md"), "crate rules");
    let workspace = repo.join("crates").join("core");
    fs::create_dir_all(&workspace).expect("workspace");
    write(&workspace.join("EULER.md"), "leaf rules");

    let bootstrap = admitted(&workspace);
    assert_eq!(
        source_paths(&bootstrap),
        vec!["EULER.md", "crates/EULER.md", "crates/core/EULER.md"]
    );
    assert_eq!(
        manifest_sources(&bootstrap)
            .iter()
            .map(|(_, content)| content.as_str())
            .collect::<Vec<_>>(),
        vec!["root rules", "crate rules", "leaf rules"]
    );
}

#[test]
fn git_file_marks_a_worktree_root_and_its_contents_are_not_followed() {
    let temp = tempfile::tempdir().expect("temp");
    let elsewhere = temp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("elsewhere");
    write(&elsewhere.join("EULER.md"), "outside content");
    let worktree = temp.path().join("wt");
    fs::create_dir_all(&worktree).expect("worktree");
    write(
        &worktree.join(".git"),
        format!("gitdir: {}\n", elsewhere.display()),
    );
    write(&worktree.join("EULER.md"), "worktree rules");

    let bootstrap = admitted(&worktree);
    assert_eq!(source_paths(&bootstrap), vec!["EULER.md"]);
    let (_, content) = &manifest_sources(&bootstrap)[0];
    assert_eq!(content, "worktree rules");
}

#[cfg(unix)]
#[test]
fn symlinked_git_entry_is_not_a_root_marker() {
    let temp = tempfile::tempdir().expect("temp");
    let outer = temp.path().join("outer");
    git_dir(&outer);
    write(&outer.join("EULER.md"), "outer rules");
    let mid = outer.join("mid");
    fs::create_dir_all(&mid).expect("mid");
    let target = temp.path().join("real-git");
    fs::create_dir_all(&target).expect("target");
    std::os::unix::fs::symlink(&target, mid.join(".git")).expect("symlink");
    write(&mid.join("EULER.md"), "mid rules");

    let bootstrap = admitted(&mid);
    // The symlinked `.git` in `mid` is ignored; the chain reaches `outer`.
    assert_eq!(source_paths(&bootstrap), vec!["EULER.md", "mid/EULER.md"]);
}

#[test]
fn nested_repository_starts_a_new_boundary() {
    let temp = tempfile::tempdir().expect("temp");
    let outer = temp.path().join("outer");
    git_dir(&outer);
    write(&outer.join("EULER.md"), "outer rules");
    let inner = outer.join("inner");
    git_dir(&inner);
    write(&inner.join("EULER.md"), "inner rules");

    let bootstrap = admitted(&inner);
    assert_eq!(source_paths(&bootstrap), vec!["EULER.md"]);
    let (_, content) = &manifest_sources(&bootstrap)[0];
    assert_eq!(content, "inner rules");
}

#[test]
fn submodule_git_file_starts_a_new_boundary() {
    let temp = tempfile::tempdir().expect("temp");
    let outer = temp.path().join("outer");
    git_dir(&outer);
    write(&outer.join("EULER.md"), "outer rules");
    let sub = outer.join("sub");
    fs::create_dir_all(&sub).expect("sub");
    write(&sub.join(".git"), "gitdir: ../.git/modules/sub\n");
    write(&sub.join("EULER.md"), "sub rules");

    let bootstrap = admitted(&sub);
    assert_eq!(source_paths(&bootstrap), vec!["EULER.md"]);
    let (_, content) = &manifest_sources(&bootstrap)[0];
    assert_eq!(content, "sub rules");
}

#[test]
fn without_a_git_marker_the_chain_is_the_workspace_alone() {
    let temp = tempfile::tempdir().expect("temp");
    write(&temp.path().join("EULER.md"), "parent rules");
    let workspace = temp.path().join("ws");
    fs::create_dir_all(&workspace).expect("ws");
    write(&workspace.join("EULER.md"), "workspace rules");

    let bootstrap = admitted(&workspace);
    assert_eq!(source_paths(&bootstrap), vec!["EULER.md"]);
    let (_, content) = &manifest_sources(&bootstrap)[0];
    assert_eq!(content, "workspace rules");
}

#[test]
fn discovery_is_independent_of_version_control_state() {
    // An ignored/untracked file loads exactly like a tracked one: discovery
    // reads the working tree only. (No git objects exist here at all.)
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    write(&repo.join(".gitignore"), "EULER.md\n");
    write(&repo.join("EULER.md"), "ignored but admitted");

    let bootstrap = admitted(&repo);
    assert_eq!(source_paths(&bootstrap), vec!["EULER.md"]);
}

fn nested_chain(temp: &Path, depth: usize) -> PathBuf {
    let mut dir = temp.to_path_buf();
    for level in 0..depth {
        dir = dir.join(format!("d{level}"));
    }
    fs::create_dir_all(&dir).expect("nested chain");
    dir
}

#[test]
fn chain_depth_bound_cap_and_cap_plus_one() {
    // Marker 31 ancestors above the workspace: chain length 32, admitted.
    let temp = tempfile::tempdir().expect("temp");
    let root = temp.path().join("repo");
    git_dir(&root);
    write(&root.join("EULER.md"), "root rules");
    let workspace = nested_chain(&root, 31);
    let bootstrap = admitted(&workspace);
    assert_eq!(source_paths(&bootstrap), vec!["EULER.md"]);
    assert!(!reasons(&bootstrap).contains(&"chain_depth_exceeded".to_owned()));

    // Marker 32 ancestors above: outside the bound, chain falls back to the
    // workspace alone with a typed diagnostic.
    let workspace = nested_chain(&root, 32);
    let bootstrap = admitted(&workspace);
    assert!(source_paths(&bootstrap).is_empty());
    assert!(reasons(&bootstrap).contains(&"chain_depth_exceeded".to_owned()));
}

// ---------------------------------------------------------------------------
// Containment: exact case, symlinks, non-regular files
// ---------------------------------------------------------------------------

#[test]
fn near_miss_casing_is_diagnosed_but_never_loaded() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    write(&repo.join("euler.md"), "wrong case");

    let bootstrap = admitted(&repo);
    assert!(source_paths(&bootstrap).is_empty());
    assert!(reasons(&bootstrap).contains(&"case_mismatch".to_owned()));
    assert!(bootstrap.manifest.expect("manifest").sources.is_empty());
}

#[cfg(unix)]
#[test]
fn symlinked_euler_md_is_rejected_not_followed() {
    let temp = tempfile::tempdir().expect("temp");
    let secret = temp.path().join("outside-secret.md");
    write(&secret, "outside the workspace");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    std::os::unix::fs::symlink(&secret, repo.join("EULER.md")).expect("symlink");

    let bootstrap = admitted(&repo);
    assert!(source_paths(&bootstrap).is_empty());
    assert!(reasons(&bootstrap).contains(&"symlink_rejected".to_owned()));
    let payload = serde_json::to_string(&bootstrap.snapshot_payload()).expect("payload");
    assert!(!payload.contains("outside the workspace"));
}

#[cfg(unix)]
#[test]
fn fifo_is_rejected_without_blocking_startup() {
    use std::os::unix::ffi::OsStrExt;
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    let fifo = repo.join("EULER.md");
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("cstring");
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) }, 0);

    let bootstrap = admitted(&repo);
    assert!(source_paths(&bootstrap).is_empty());
    assert!(reasons(&bootstrap).contains(&"not_regular_file".to_owned()));
}

#[test]
fn directory_named_euler_md_is_rejected() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    fs::create_dir_all(repo.join("EULER.md")).expect("dir");

    let bootstrap = admitted(&repo);
    assert!(source_paths(&bootstrap).is_empty());
    // Opened via the no-follow candidate path and rejected as non-regular
    // (some platforms fail the open itself; either way nothing is admitted).
    assert!(
        reasons(&bootstrap).contains(&"not_regular_file".to_owned())
            || reasons(&bootstrap).contains(&"io_error".to_owned())
    );
}

#[test]
fn invalid_utf8_source_is_omitted_whole() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    write(&repo.join("EULER.md"), [0xff, 0xfe, 0x00, 0x41]);

    let bootstrap = admitted(&repo);
    assert!(source_paths(&bootstrap).is_empty());
    assert!(reasons(&bootstrap).contains(&"invalid_utf8".to_owned()));
}

// ---------------------------------------------------------------------------
// Stable-read protocol
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn source_changing_across_both_read_attempts_is_omitted() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    let file = repo.join("EULER.md");
    write(&file, "v0");

    let target = file.clone();
    let mut version = 0u32;
    super::discovery::test_hook::set_after_read(Some(Box::new(move || {
        version += 1;
        fs::write(&target, format!("mutated {version} bytes longer")).expect("mutate");
    })));
    let bootstrap = admitted(&repo);
    super::discovery::test_hook::set_after_read(None);

    assert!(source_paths(&bootstrap).is_empty());
    assert!(reasons(&bootstrap).contains(&"changed_during_read".to_owned()));
}

#[cfg(unix)]
#[test]
fn source_stable_on_retry_is_admitted_with_the_reread_bytes() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    let file = repo.join("EULER.md");
    write(&file, "before mutation");

    let target = file.clone();
    let mut fired = false;
    super::discovery::test_hook::set_after_read(Some(Box::new(move || {
        if !fired {
            fired = true;
            fs::write(&target, "after mutation!!").expect("mutate");
        }
    })));
    let bootstrap = admitted(&repo);
    super::discovery::test_hook::set_after_read(None);

    assert_eq!(source_paths(&bootstrap), vec!["EULER.md"]);
    let (_, content) = &manifest_sources(&bootstrap)[0];
    assert_eq!(content, "after mutation!!");
    assert!(!reasons(&bootstrap).contains(&"changed_during_read".to_owned()));
}

// ---------------------------------------------------------------------------
// Numeric bounds: cap - 1 / cap / cap + 1
// ---------------------------------------------------------------------------

#[test]
fn per_file_bound_boundary_behavior() {
    for (size, admitted_expected) in [
        (MAX_EULER_MD_BYTES - 1, true),
        (MAX_EULER_MD_BYTES, true),
        (MAX_EULER_MD_BYTES + 1, false),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let repo = temp.path().join("repo");
        git_dir(&repo);
        write(&repo.join("EULER.md"), "x".repeat(size));
        let bootstrap = admitted(&repo);
        assert_eq!(
            source_paths(&bootstrap).len(),
            usize::from(admitted_expected),
            "size {size}"
        );
        assert_eq!(
            reasons(&bootstrap).contains(&"source_too_large".to_owned()),
            !admitted_expected,
            "size {size}"
        );
    }
}

#[test]
fn combined_bound_prefers_more_specific_sources_and_omits_whole_files() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    let quarter = MAX_COMBINED_EULER_MD_BYTES / 4; // 16 KiB, under per-file cap
    write(&repo.join("EULER.md"), "r".repeat(quarter * 2));
    write(&repo.join("a").join("EULER.md"), "a".repeat(quarter * 2));
    let workspace = repo.join("a").join("b");
    fs::create_dir_all(&workspace).expect("ws");
    write(&workspace.join("EULER.md"), "b".repeat(quarter + 1));

    let bootstrap = admitted(&workspace);
    // Deepest-first admission: b (quarter+1) + a (2*quarter) fit; adding the
    // root's 2*quarter would exceed the aggregate cap, so the root file is
    // omitted whole and the accepted set renders root-first.
    assert_eq!(source_paths(&bootstrap), vec!["a/EULER.md", "a/b/EULER.md"]);
    assert!(reasons(&bootstrap).contains(&"combined_limit_exceeded".to_owned()));
}

#[test]
fn combined_bound_boundary_exact_fit_is_admitted() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    let half = MAX_COMBINED_EULER_MD_BYTES / 2; // equals per-file cap
    write(&repo.join("EULER.md"), "r".repeat(half));
    let workspace = repo.join("a");
    fs::create_dir_all(&workspace).expect("ws");
    write(&workspace.join("EULER.md"), "a".repeat(half));

    let bootstrap = admitted(&workspace);
    assert_eq!(source_paths(&bootstrap), vec!["EULER.md", "a/EULER.md"]);
    assert!(!reasons(&bootstrap).contains(&"combined_limit_exceeded".to_owned()));
}

#[test]
fn source_count_bound_boundary_behavior() {
    for (dirs, expected_admitted, expect_diagnostic) in [
        (MAX_EULER_MD_SOURCES - 1, MAX_EULER_MD_SOURCES - 1, false),
        (MAX_EULER_MD_SOURCES, MAX_EULER_MD_SOURCES, false),
        (MAX_EULER_MD_SOURCES + 2, MAX_EULER_MD_SOURCES, true),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let repo = temp.path().join("repo");
        git_dir(&repo);
        write(&repo.join("EULER.md"), "level 0");
        let mut dir = repo.clone();
        for level in 1..dirs {
            dir = dir.join(format!("d{level}"));
            fs::create_dir_all(&dir).expect("dir");
            write(&dir.join("EULER.md"), format!("level {level}"));
        }
        let bootstrap = admitted(&dir);
        assert_eq!(source_paths(&bootstrap).len(), expected_admitted, "{dirs}");
        assert_eq!(
            reasons(&bootstrap).contains(&"source_count_exceeded".to_owned()),
            expect_diagnostic,
            "{dirs}"
        );
        if expect_diagnostic {
            // More-specific sources won: the shallowest files were omitted.
            assert!(!source_paths(&bootstrap).contains(&"EULER.md".to_owned()));
        }
    }
}

// ---------------------------------------------------------------------------
// Determinism, digests, redaction, privacy
// ---------------------------------------------------------------------------

#[test]
fn preflight_is_deterministic() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    write(&repo.join("EULER.md"), "same rules");

    let first = admitted(&repo);
    let second = admitted(&repo);
    assert_eq!(first.candidate_digest, second.candidate_digest);
    assert_eq!(first.manifest, second.manifest);
    assert_eq!(first.diagnostics, second.diagnostics);
}

#[test]
fn candidate_digest_is_portable_across_checkouts_and_workspace_identity_is_not() {
    let temp = tempfile::tempdir().expect("temp");
    for name in ["checkout-a", "checkout-b"] {
        let repo = temp.path().join(name);
        git_dir(&repo);
        write(&repo.join("EULER.md"), "shared team rules");
    }
    let a = admitted(&temp.path().join("checkout-a"));
    let b = admitted(&temp.path().join("checkout-b"));
    assert_eq!(a.candidate_digest, b.candidate_digest);
    assert_ne!(a.workspace_identity_digest, b.workspace_identity_digest);
}

#[test]
fn changed_content_changes_the_candidate_digest() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    write(&repo.join("EULER.md"), "version one");
    let before = admitted(&repo);
    write(&repo.join("EULER.md"), "version two");
    let after = admitted(&repo);
    assert_ne!(before.candidate_digest, after.candidate_digest);
}

#[test]
fn seeded_secrets_are_redacted_before_digest_events_and_manifest() {
    let secret = "sk-test-1234567890abcdef1234567890";
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    write(
        &repo.join("EULER.md"),
        format!("deploy: use {secret} to authenticate"),
    );
    let redactor = SecretRedactor::new();
    redactor.add_value(secret);

    let bootstrap =
        ProjectContextBootstrap::admitted_for_tests(&repo, &redactor).expect("preflight");
    let manifest_json = bootstrap
        .manifest
        .as_ref()
        .expect("manifest")
        .to_canonical_json();
    assert!(!manifest_json.contains(secret));
    assert!(manifest_json.contains("[redacted-secret]"));
    let payload = serde_json::to_string(&bootstrap.snapshot_payload()).expect("payload");
    assert!(!payload.contains(secret));
    // The digest commits to the post-redaction bytes: a bootstrap over the
    // already-redacted content produces the same digest.
    write(
        &repo.join("EULER.md"),
        "deploy: use [redacted-secret] to authenticate",
    );
    let re_read = admitted(&repo);
    assert_eq!(re_read.candidate_digest, bootstrap.candidate_digest);
}

#[test]
fn dormant_bootstrap_discloses_no_content_and_no_content_lengths() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    write(&repo.join("EULER.md"), "private repository text");
    // Near-miss casing lives in a subdirectory: on a case-insensitive
    // filesystem a same-directory variant would collide with the real file.
    let workspace = repo.join("sub");
    fs::create_dir_all(&workspace).expect("sub");
    write(&workspace.join("euler.md"), "near miss");

    let bootstrap = dormant(&workspace);
    assert_eq!(bootstrap.status(), ProjectContextStatus::Disabled);
    assert!(bootstrap.manifest.is_none());
    let payload = bootstrap.snapshot_payload();
    assert!(payload.get("manifest").is_none());
    assert!(payload.get("manifest_len").is_none());
    let serialized = serde_json::to_string(&payload).expect("payload");
    assert!(!serialized.contains("private repository text"));
    // Identities, counts, digest, and reason codes remain.
    assert_eq!(payload["status"], serde_json::json!("disabled"));
    assert_eq!(payload["policy"], serde_json::json!("off"));
    assert_eq!(
        payload["source_identities"],
        serde_json::json!(["EULER.md"])
    );
    assert_eq!(
        payload["diagnostic_reason_counts"],
        serde_json::json!({"case_mismatch": 1})
    );
    assert_eq!(
        payload["candidate_digest"],
        admitted(&workspace).candidate_digest
    );
    // The summary and diagnostics stay content-free too.
    let summary = serde_json::to_string(&bootstrap.session_start_summary()).expect("summary");
    assert!(!summary.contains("private repository text"));
}

// ---------------------------------------------------------------------------
// Fold: latest-authoritative, tombstones, malformation
// ---------------------------------------------------------------------------

#[test]
fn admitted_snapshot_folds_to_one_pinned_item_with_framed_bytes() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    write(&repo.join("EULER.md"), "build with cargo");
    let bootstrap = admitted(&repo);
    let events = bootstrap_events(&bootstrap);

    let fold = fold_project_context(&events).expect("fold");
    let pinned = fold.admitted().expect("pinned");
    assert_eq!(pinned.candidate_digest, bootstrap.candidate_digest);
    assert!(pinned.rendered.contains("    build with cargo"));
    assert_eq!(pinned.snapshot_event_id, events[1].id);
    assert_eq!(
        pinned.rendered_digest,
        super::digest::rendered_digest_v1(&pinned.rendered)
    );
}

#[test]
fn disabled_snapshot_folds_to_a_tombstone() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    write(&repo.join("EULER.md"), "content");
    let events = bootstrap_events(&dormant(&repo));
    assert_eq!(
        fold_project_context(&events).expect("fold"),
        ProjectContextFold::Disabled
    );
}

#[test]
fn later_disabled_snapshot_tombstones_an_earlier_admitted_one() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    write(&repo.join("EULER.md"), "content");
    let mut events = bootstrap_events(&admitted(&repo));
    events.push(EventEnvelope::new(
        "session",
        "root",
        None,
        EventKind::PROJECT_CONTEXT_SNAPSHOT,
        dormant(&repo).snapshot_payload(),
    ));
    assert_eq!(
        fold_project_context(&events).expect("fold"),
        ProjectContextFold::Disabled
    );
}

#[test]
fn no_snapshot_folds_absent() {
    assert_eq!(
        fold_project_context(&[]).expect("fold"),
        ProjectContextFold::Absent
    );
}

#[test]
fn malformed_latest_snapshot_rejects_and_never_resurrects_an_older_one() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    write(&repo.join("EULER.md"), "content");
    let mut events = bootstrap_events(&admitted(&repo));
    let mut bad = admitted(&repo).snapshot_payload();
    bad.insert("schema_version".to_owned(), 99.into());
    events.push(EventEnvelope::new(
        "session",
        "root",
        None,
        EventKind::PROJECT_CONTEXT_SNAPSHOT,
        bad,
    ));
    assert!(fold_project_context(&events).is_err());
}

#[test]
fn tampered_manifest_or_length_rejects() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    write(&repo.join("EULER.md"), "content");
    let bootstrap = admitted(&repo);

    // Content tamper: digest mismatch.
    let mut payload = bootstrap.snapshot_payload();
    let manifest = payload["manifest"].as_str().expect("manifest").to_owned();
    let tampered = manifest.replace("content", "tampered");
    payload.insert("manifest_len".to_owned(), tampered.len().into());
    payload.insert("manifest".to_owned(), tampered.into());
    let events = vec![EventEnvelope::new(
        "session",
        "root",
        None,
        EventKind::PROJECT_CONTEXT_SNAPSHOT,
        payload,
    )];
    assert!(fold_project_context(&events).is_err());

    // Length tamper.
    let mut payload = bootstrap.snapshot_payload();
    payload.insert("manifest_len".to_owned(), 1.into());
    let events = vec![EventEnvelope::new(
        "session",
        "root",
        None,
        EventKind::PROJECT_CONTEXT_SNAPSHOT,
        payload,
    )];
    assert!(fold_project_context(&events).is_err());

    // Unknown status never falls through to admitted or disabled.
    let mut payload = bootstrap.snapshot_payload();
    payload.insert("status".to_owned(), "mystery".into());
    let events = vec![EventEnvelope::new(
        "session",
        "root",
        None,
        EventKind::PROJECT_CONTEXT_SNAPSHOT,
        payload,
    )];
    assert!(fold_project_context(&events).is_err());
}

// ---------------------------------------------------------------------------
// Bootstrap shape validation
// ---------------------------------------------------------------------------

fn plain_session_start() -> EventEnvelope {
    EventEnvelope::new(
        "session",
        "root",
        None,
        EventKind::SESSION_START,
        euler_event::object([("provider", "fixture".into()), ("model", "echo".into())]),
    )
}

#[test]
fn legacy_shape_without_summary_or_snapshot_is_valid() {
    assert!(validate_bootstrap_shape(&[plain_session_start()]).is_ok());
    assert!(validate_bootstrap_shape(&[]).is_ok());
}

#[test]
fn complete_bootstrap_shape_is_valid() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    write(&repo.join("EULER.md"), "content");
    let workspace = repo.join("sub");
    fs::create_dir_all(&workspace).expect("sub");
    write(&workspace.join("euler.md"), "near miss");
    let events = bootstrap_events(&dormant(&workspace));
    assert!(events.len() >= 3, "bootstrap has diagnostics");
    validate_bootstrap_shape(&events).expect("valid shape");
}

#[test]
fn partial_and_mixed_bootstrap_shapes_fail_closed() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    write(&repo.join("EULER.md"), "content");
    let workspace = repo.join("sub");
    fs::create_dir_all(&workspace).expect("sub");
    write(&workspace.join("euler.md"), "near miss");
    let events = bootstrap_events(&dormant(&workspace));

    // Summary without snapshot.
    assert!(validate_bootstrap_shape(&events[..1]).is_err());
    // Snapshot without summary.
    let mut no_summary = events.clone();
    no_summary[0] = plain_session_start();
    assert!(validate_bootstrap_shape(&no_summary).is_err());
    // Declared diagnostics missing.
    assert!(validate_bootstrap_shape(&events[..2]).is_err());
    // Duplicated snapshot.
    let mut duplicated = events.clone();
    duplicated.push(events[1].clone());
    assert!(validate_bootstrap_shape(&duplicated).is_err());
    // Stray diagnostic outside the bootstrap.
    let mut stray = events.clone();
    stray.push(events[2].clone());
    assert!(validate_bootstrap_shape(&stray).is_err());
    // Diagnostic citing a different snapshot.
    let mut miscited = events.clone();
    miscited[2]
        .payload
        .insert("snapshot_event_id".to_owned(), "01J-other".into());
    assert!(validate_bootstrap_shape(&miscited).is_err());
}

// ---------------------------------------------------------------------------
// Workspace identity
// ---------------------------------------------------------------------------

#[test]
fn workspace_identity_accepts_the_recorded_workspace_and_rejects_another() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    write(&repo.join("EULER.md"), "content");
    let other = temp.path().join("other");
    fs::create_dir_all(&other).expect("other");
    let events = bootstrap_events(&dormant(&repo));

    verify_workspace_identity(&events, &repo).expect("same workspace verifies");
    assert_eq!(
        verify_workspace_identity(&events, &other),
        Err(WorkspaceIdentityIssue::Mismatch)
    );
    assert_eq!(
        verify_workspace_identity(&events, &temp.path().join("missing")),
        Err(WorkspaceIdentityIssue::Unresolvable)
    );
}

#[test]
fn workspace_identity_with_unknown_algorithm_or_missing_record_is_unusable() {
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    git_dir(&repo);
    let bootstrap = dormant(&repo);

    let mut payload = bootstrap.snapshot_payload();
    payload.insert(
        "workspace_identity".to_owned(),
        serde_json::json!({"algorithm": "future-host", "version": 9, "digest": "abc"}),
    );
    let events = vec![EventEnvelope::new(
        "session",
        "root",
        None,
        EventKind::PROJECT_CONTEXT_SNAPSHOT,
        payload,
    )];
    assert_eq!(
        verify_workspace_identity(&events, &repo),
        Err(WorkspaceIdentityIssue::Unusable)
    );

    let mut payload = bootstrap.snapshot_payload();
    payload.remove("workspace_identity");
    let events = vec![EventEnvelope::new(
        "session",
        "root",
        None,
        EventKind::PROJECT_CONTEXT_SNAPSHOT,
        payload,
    )];
    assert_eq!(
        verify_workspace_identity(&events, &repo),
        Err(WorkspaceIdentityIssue::Unusable)
    );

    // Sessions without snapshots verify trivially.
    verify_workspace_identity(&[plain_session_start()], &repo).expect("legacy ok");
}
