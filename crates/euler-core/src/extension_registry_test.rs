use super::*;
use euler_sdk::extension_package::LinkInventoryError;
use euler_sdk::{load_extension_package, EXTENSION_MANIFEST_FILE};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{symlink, PermissionsExt};
use std::sync::Arc;
use std::thread;

#[test]
fn registry_absent_state_defaults_disabled_and_persists_changes() {
    let (_temp, registry) = registry();

    assert_eq!(
        registry.state("session-export").expect("initial state"),
        ExtensionEnablement::Disabled
    );

    assert_eq!(
        registry.enable("session-export").expect("enable"),
        ExtensionEnablement::Enabled
    );
    assert_eq!(
        ExtensionRegistry::new(registry.home().clone())
            .expect("registry")
            .state("session-export")
            .expect("enabled state"),
        ExtensionEnablement::Enabled
    );

    registry.disable("session-export").expect("disable");
    assert_eq!(
        registry.state("session-export").expect("disabled state"),
        ExtensionEnablement::Disabled
    );
}

#[test]
fn registry_ignores_torn_tail_without_enabling_it() {
    let (_temp, registry) = registry();
    write_state_log(
        &registry,
        r#"{"v":1,"op":"disable","id":"session-export","ts_ms":1}
{"v":1,"op":"enable","id":"session-export","ts_ms":2}"#,
    );

    assert_eq!(
        registry.state("session-export").expect("state"),
        ExtensionEnablement::Disabled
    );
}

#[test]
fn registry_corrupt_accepted_line_errors_and_blocks_write() {
    let (_temp, registry) = registry();
    write_state_log(&registry, "not json\n");

    assert!(matches!(
        registry.state("session-export").expect_err("corrupt state"),
        ExtensionRegistryError::InvalidStateLine(_)
    ));
    assert!(matches!(
        registry
            .enable("session-export")
            .expect_err("corrupt write blocked"),
        ExtensionRegistryError::InvalidStateLine(_)
    ));
    assert_eq!(
        fs::read_to_string(registry.state_log_path()).expect("state log"),
        "not json\n"
    );
}

#[test]
fn registry_audit_error_report_serializes_exact_core_schema() {
    let error = ExtensionRegistryError::LinkInventory(LinkInventoryError::InvalidExtensionId {
        id: "Bad".to_owned(),
    });

    assert_eq!(
        ExtensionAuditErrorCode::from_registry_error(&error),
        ExtensionAuditErrorCode::RegistryInventoryInvalid
    );
    assert_eq!(
        serde_json::to_value(ExtensionAuditErrorReport::from_registry_error(&error))
            .expect("audit error json"),
        serde_json::json!({
            "schema_version": 1,
            "error": {
                "code": "registry-inventory-invalid",
                "message": "extension registry audit failed"
            }
        })
    );
}

#[test]
fn registry_unsupported_version_errors_and_blocks_write() {
    let (_temp, registry) = registry();
    write_state_log(
        &registry,
        r#"{"v":2,"op":"enable","id":"session-export","ts_ms":1}
"#,
    );

    assert!(matches!(
        registry.state("session-export").expect_err("bad version"),
        ExtensionRegistryError::UnsupportedVersion { version: 2 }
    ));
    assert!(matches!(
        registry
            .disable("session-export")
            .expect_err("bad version blocks write"),
        ExtensionRegistryError::UnsupportedVersion { version: 2 }
    ));
    assert_eq!(
        fs::read_to_string(registry.state_log_path()).expect("state log"),
        r#"{"v":2,"op":"enable","id":"session-export","ts_ms":1}
"#
    );
}

#[test]
fn registry_invalid_persisted_id_errors_and_blocks_write() {
    let (_temp, registry) = registry();
    write_state_log(
        &registry,
        r#"{"v":1,"op":"enable","id":"Bad","ts_ms":1}
"#,
    );

    assert!(matches!(
        registry.state("session-export").expect_err("bad id"),
        ExtensionRegistryError::InvalidExtensionId { id } if id == "Bad"
    ));
    assert!(matches!(
        registry
            .enable("session-export")
            .expect_err("bad id blocks write"),
        ExtensionRegistryError::InvalidExtensionId { id } if id == "Bad"
    ));
    assert_eq!(
        fs::read_to_string(registry.state_log_path()).expect("state log"),
        r#"{"v":1,"op":"enable","id":"Bad","ts_ms":1}
"#
    );
}

#[cfg(unix)]
#[test]
fn registry_uses_private_permissions_for_dir_state_and_lock() {
    let (_temp, registry) = registry();
    registry.enable("session-export").expect("enable");
    let _lock = registry.acquire_lock().expect("lock");

    assert_eq!(mode(&registry.home().extensions_dir()), 0o700);
    assert_eq!(mode(&registry.state_log_path()), 0o600);
    assert_eq!(mode(&registry.state_log_lock_path()), 0o600);
}

#[cfg(unix)]
#[test]
fn registry_rejects_symlinked_registry_root_without_writing_through() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = EulerHome::from_root(temp.path().join(".euler")).expect("home");
    let target = tempfile::tempdir().expect("target dir");
    symlink(target.path(), home.extensions_dir()).expect("registry root symlink");

    let error = ExtensionRegistry::new(home).expect_err("symlinked registry root");

    assert!(matches!(error, ExtensionRegistryError::Io(_)));
    assert!(error
        .to_string()
        .contains("extension registry path must not be a symlink"));
    assert!(!error
        .to_string()
        .contains(target.path().to_string_lossy().as_ref()));
    assert!(target
        .path()
        .read_dir()
        .expect("target entries")
        .next()
        .is_none());
}

#[cfg(unix)]
#[test]
fn registry_rejects_dangling_symlinked_registry_root() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = EulerHome::from_root(temp.path().join(".euler")).expect("home");
    let target = temp.path().join("missing-registry-root");
    symlink(&target, home.extensions_dir()).expect("dangling registry root symlink");

    let error = ExtensionRegistry::new(home).expect_err("dangling registry root");

    assert!(matches!(error, ExtensionRegistryError::Io(_)));
    assert!(error
        .to_string()
        .contains("extension registry path must not be a symlink"));
    assert!(!error
        .to_string()
        .contains(target.to_string_lossy().as_ref()));
}

#[cfg(unix)]
#[test]
fn registry_rejects_registry_root_swapped_to_symlink_before_mutation() {
    let (_temp, registry) = registry();
    let target = tempfile::tempdir().expect("target dir");
    fs::remove_dir_all(registry.home().extensions_dir()).expect("remove registry dir");
    symlink(target.path(), registry.home().extensions_dir()).expect("registry root symlink");

    let error = registry
        .enable("session-export")
        .expect_err("symlinked registry root");

    assert!(matches!(error, ExtensionRegistryError::Io(_)));
    assert!(target
        .path()
        .read_dir()
        .expect("target entries")
        .next()
        .is_none());
}

#[cfg(unix)]
#[test]
fn registry_audit_rejects_symlinked_registry_root_without_creating_state() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = EulerHome::from_root(temp.path().join(".euler")).expect("home");
    let target = tempfile::tempdir().expect("target dir");
    symlink(target.path(), home.extensions_dir()).expect("registry root symlink");
    let registry = ExtensionRegistry::open_read_only(home);

    let error = registry.audit().expect_err("symlinked registry root");

    assert!(matches!(error, ExtensionRegistryError::Io(_)));
    assert!(target
        .path()
        .read_dir()
        .expect("target entries")
        .next()
        .is_none());
}

#[cfg(unix)]
#[test]
fn registry_rejects_symlinked_state_log_without_writing_through() {
    let (_temp, registry) = registry();
    let target = tempfile::NamedTempFile::new().expect("target file");
    fs::write(target.path(), b"sentinel\n").expect("target content");
    symlink(target.path(), registry.state_log_path()).expect("state log symlink");

    let error = registry
        .enable("session-export")
        .expect_err("symlinked state log");

    assert!(matches!(error, ExtensionRegistryError::Io(_)));
    assert!(error
        .to_string()
        .contains("extension registry file path must not be a symlink"));
    assert!(!error
        .to_string()
        .contains(target.path().to_string_lossy().as_ref()));
    assert_eq!(
        fs::read(target.path()).expect("target after"),
        b"sentinel\n"
    );
}

#[cfg(unix)]
#[test]
fn registry_rejects_symlinked_state_lock_without_chmod_or_locking_target() {
    let (_temp, registry) = registry();
    let target = tempfile::NamedTempFile::new().expect("target file");
    fs::write(target.path(), b"lock sentinel\n").expect("target content");
    fs::set_permissions(target.path(), fs::Permissions::from_mode(0o644)).expect("target mode");
    symlink(target.path(), registry.state_log_lock_path()).expect("state lock symlink");

    let error = registry
        .enable("session-export")
        .expect_err("symlinked state lock");

    assert!(matches!(error, ExtensionRegistryError::Io(_)));
    assert!(error
        .to_string()
        .contains("extension registry file path must not be a symlink"));
    assert!(!error
        .to_string()
        .contains(target.path().to_string_lossy().as_ref()));
    assert_eq!(
        fs::read(target.path()).expect("target after"),
        b"lock sentinel\n"
    );
    assert_eq!(mode(target.path()), 0o644);
}

#[cfg(unix)]
#[test]
fn registry_rejects_symlinked_link_inventory_without_following_or_replacing() {
    let (_temp, registry) = registry();
    let target = tempfile::NamedTempFile::new().expect("target file");
    fs::write(target.path(), b"inventory sentinel\n").expect("target content");
    symlink(target.path(), registry.link_inventory_path()).expect("inventory symlink");
    let package_dir = extension_dir("example-extension");

    let error = registry
        .link_package(load_extension_package(package_dir.path()).expect("package"))
        .expect_err("symlinked inventory");

    assert!(matches!(error, ExtensionRegistryError::Io(_)));
    assert_eq!(
        fs::read(target.path()).expect("target after"),
        b"inventory sentinel\n"
    );
    assert!(registry.link_inventory_path().is_symlink());
}

#[cfg(unix)]
#[test]
fn registry_rejects_symlinked_link_inventory_tmp_without_writing_through() {
    let (_temp, registry) = registry();
    let target = tempfile::NamedTempFile::new().expect("target file");
    fs::write(target.path(), b"tmp sentinel\n").expect("target content");
    symlink(target.path(), registry.link_inventory_tmp_path()).expect("inventory tmp symlink");
    let package_dir = extension_dir("example-extension");

    let error = registry
        .link_package(load_extension_package(package_dir.path()).expect("package"))
        .expect_err("symlinked inventory tmp");

    assert!(matches!(error, ExtensionRegistryError::Io(_)));
    assert_eq!(
        fs::read(target.path()).expect("target after"),
        b"tmp sentinel\n"
    );
    assert!(registry.link_inventory_tmp_path().is_symlink());
    assert!(!registry.link_inventory_path().exists());
}

#[cfg(unix)]
#[test]
fn private_file_write_distinguishes_failure_after_visible_rename() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("state.json");
    let tmp = dir.path().join("state.json.tmp");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o300))
        .expect("make directory writable but unreadable");

    let error = write_private_file(&path, &tmp, b"new state\n")
        .expect_err("parent directory sync should fail");

    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))
        .expect("restore directory permissions");
    assert!(
        matches!(error, PrivateFileWriteError::AfterRename(_)),
        "the replacement is visible even though its directory sync failed: {error:?}"
    );
    assert_eq!(
        fs::read(&path).expect("visible replacement"),
        b"new state\n"
    );
}

#[cfg(unix)]
#[test]
fn registry_install_rejects_symlinked_installed_root_without_deleting_target() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    let package = load_extension_package(package_dir.path()).expect("package");
    let outside = tempfile::tempdir().expect("outside");
    let outside_snapshot = outside
        .path()
        .join(&package.descriptor.id)
        .join(&package.manifest_sha256);
    fs::create_dir_all(&outside_snapshot).expect("outside snapshot");
    let sentinel = outside_snapshot.join("sentinel");
    fs::write(&sentinel, b"must survive\n").expect("sentinel");
    symlink(outside.path(), registry.installed_extensions_dir()).expect("installed root symlink");

    let error = registry
        .install_package(package)
        .expect_err("symlinked installed root");

    assert!(matches!(error, ExtensionRegistryError::Io(_)));
    assert!(error
        .to_string()
        .contains("installed extension path must not be a symlink"));
    assert_eq!(
        fs::read(&sentinel).expect("outside sentinel after rejected install"),
        b"must survive\n"
    );
    assert!(registry.installed_extensions_dir().is_symlink());
    assert!(registry
        .linked_extension("example-extension")
        .expect("inventory")
        .is_none());
}

#[cfg(unix)]
#[test]
fn registry_install_replaces_orphan_snapshot_without_following_symlinked_tmp() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    let package = load_extension_package(package_dir.path()).expect("package");
    let manifest_bytes = package.manifest_bytes.clone();
    let installed_dir =
        registry.installed_package_dir(&package.descriptor.id, &package.manifest_sha256);
    fs::create_dir_all(&installed_dir).expect("installed dir");
    let tmp = installed_dir.join(format!("{EXTENSION_MANIFEST_FILE}.tmp"));
    let target = tempfile::NamedTempFile::new().expect("target file");
    fs::write(target.path(), b"manifest tmp sentinel\n").expect("target content");
    symlink(target.path(), &tmp).expect("manifest tmp symlink");

    // The pre-existing dir has no inventory entry, so install treats it as a
    // crash orphan: the whole dir (including the symlink, unfollowed) is
    // removed and replaced by a fresh snapshot.
    let installed = registry.install_package(package).expect("install");

    assert_eq!(installed.source_path, installed_dir);
    assert_eq!(
        fs::read(target.path()).expect("target after"),
        b"manifest tmp sentinel\n"
    );
    assert!(!tmp.exists());
    assert_eq!(
        fs::read(installed.source_path.join(EXTENSION_MANIFEST_FILE)).expect("manifest"),
        manifest_bytes
    );
}

#[cfg(unix)]
#[test]
fn registry_install_replaces_stale_regular_manifest_tmp() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    let package = load_extension_package(package_dir.path()).expect("package");
    let manifest_bytes = package.manifest_bytes.clone();
    let installed_dir =
        registry.installed_package_dir(&package.descriptor.id, &package.manifest_sha256);
    fs::create_dir_all(&installed_dir).expect("installed dir");
    let tmp = installed_dir.join(format!("{EXTENSION_MANIFEST_FILE}.tmp"));
    fs::write(&tmp, b"stale tmp").expect("stale tmp");

    let installed = registry.install_package(package).expect("install");

    assert_eq!(installed.source_path, installed_dir);
    assert_eq!(
        fs::read(installed.source_path.join(EXTENSION_MANIFEST_FILE)).expect("manifest"),
        manifest_bytes
    );
    assert!(!tmp.exists());
}

#[test]
fn registry_rejects_invalid_extension_ids() {
    let (_temp, registry) = registry();

    for id in ["", "-bad", "Bad", "bad_", "bad..id", "..", "bad/path"] {
        assert!(matches!(
            registry.state(id).expect_err("invalid id"),
            ExtensionRegistryError::InvalidExtensionId { .. }
        ));
    }
}

#[test]
fn registry_concurrent_mutations_leave_valid_jsonl() {
    let (_temp, registry) = registry();
    let registry = Arc::new(registry);
    let mut handles = Vec::new();

    for index in 0..24 {
        let registry = Arc::clone(&registry);
        handles.push(thread::spawn(move || {
            registry
                .set_enabled("session-export", index % 2 == 0)
                .expect("set enabled");
        }));
    }
    for handle in handles {
        handle.join().expect("thread");
    }

    let state = registry.state("session-export").expect("fold state");
    assert!(matches!(
        state,
        ExtensionEnablement::Enabled | ExtensionEnablement::Disabled
    ));
    let raw = fs::read_to_string(registry.state_log_path()).expect("state log");
    assert_eq!(raw.lines().count(), 24);
    for line in raw.lines() {
        let entry = serde_json::from_str::<StateEntry>(line).expect("valid entry");
        validate_entry(&entry).expect("valid state entry");
    }
}

#[test]
fn registry_links_reloads_and_unlinks_local_package() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    let package = load_extension_package(package_dir.path()).expect("package");

    let linked = registry.link_package(package).expect("link");

    assert_eq!(linked.id, "example-extension");
    assert_eq!(linked.status, LinkedExtensionStatus::NeedsReview);
    assert_eq!(
        registry
            .linked_extension("example-extension")
            .expect("lookup")
            .expect("linked")
            .manifest_sha256,
        linked.manifest_sha256
    );

    write_manifest(package_dir.path(), "example-extension", "0.2.0");
    let reloaded = registry.reload_link("example-extension").expect("reload");
    assert_eq!(reloaded.status, LinkedExtensionStatus::NeedsReview);
    assert_eq!(reloaded.descriptor.version, "0.2.0");
    assert!(reloaded.updated_ts_ms >= linked.updated_ts_ms);

    registry.unlink("example-extension").expect("unlink");
    assert!(registry
        .linked_extension("example-extension")
        .expect("lookup")
        .is_none());
    assert!(matches!(
        registry.unlink("example-extension").expect_err("gone"),
        ExtensionRegistryError::UnknownLinkedExtension { .. }
    ));
}

#[test]
fn linked_package_activation_is_explicit_and_reload_revokes_it() {
    let (_temp, registry) = registry();
    let package_dir = managed_process_extension_dir("example-extension");
    registry
        .link_package(load_extension_package(package_dir.path()).expect("package"))
        .expect("link");
    assert_eq!(
        registry
            .linked_execution_enabled("example-extension")
            .expect("initial linked activation"),
        ExtensionEnablement::Disabled
    );

    let enabled = registry
        .set_linked_execution_enabled("example-extension", true)
        .expect("enable linked package");
    assert_eq!(enabled, ExtensionEnablement::Enabled);
    assert_eq!(
        registry
            .linked_execution_enabled("example-extension")
            .expect("linked activation"),
        ExtensionEnablement::Enabled
    );
    assert_eq!(
        registry.state("example-extension").expect("general state"),
        ExtensionEnablement::Disabled,
        "linked launch consent must not enter the general enablement log"
    );
    assert!(
        !registry
            .enablement_states()
            .expect("bundled states")
            .contains_key("example-extension"),
        "linked launch consent has its own persistence owner"
    );

    let disabled = registry
        .set_linked_execution_enabled("example-extension", false)
        .expect("disable linked package");
    assert_eq!(disabled, ExtensionEnablement::Disabled);

    registry
        .set_linked_execution_enabled("example-extension", true)
        .expect("re-enable linked package");
    write_managed_process_manifest(package_dir.path(), "example-extension", "0.2.0");
    let reloaded = registry.reload_link("example-extension").expect("reload");
    assert_eq!(reloaded.status, LinkedExtensionStatus::NeedsReview);
    assert_eq!(
        registry
            .linked_execution_enabled("example-extension")
            .expect("revoked activation"),
        ExtensionEnablement::Disabled
    );
}

#[test]
fn linked_package_activation_refuses_native_and_broken_packages() {
    let (_temp, registry) = registry();
    let native_dir = extension_dir("native-extension");
    registry
        .link_package(load_extension_package(native_dir.path()).expect("native package"))
        .expect("link native package");
    assert!(matches!(
        registry
            .set_linked_execution_enabled("native-extension", true)
            .expect_err("native runtime cannot activate"),
        ExtensionRegistryError::NotManagedProcess { .. }
    ));

    let broken_dir = managed_process_extension_dir("broken-extension");
    registry
        .link_package(load_extension_package(broken_dir.path()).expect("process package"))
        .expect("link process package");
    fs::remove_file(broken_dir.path().join(EXTENSION_MANIFEST_FILE)).expect("remove manifest");
    assert_eq!(
        registry
            .reload_link("broken-extension")
            .expect("reload broken package")
            .status,
        LinkedExtensionStatus::Broken
    );
    assert_eq!(
        registry
            .set_linked_execution_enabled("broken-extension", false)
            .expect("broken package can always lose launch consent"),
        ExtensionEnablement::Disabled
    );
}

#[test]
fn registry_link_rejects_conflicting_id_or_path() {
    let (_temp, registry) = registry();
    let first_dir = extension_dir("example-extension");
    let second_dir = extension_dir("example-extension");
    let third_dir = extension_dir("other-extension");

    registry
        .link_package(load_extension_package(first_dir.path()).expect("first"))
        .expect("first link");
    let refreshed = registry
        .link_package(load_extension_package(first_dir.path()).expect("same path"))
        .expect("same path relink refresh");
    assert_eq!(refreshed.id, "example-extension");

    assert!(matches!(
        registry
            .link_package(load_extension_package(second_dir.path()).expect("second"))
            .expect_err("id conflict"),
        ExtensionRegistryError::LinkInventory(LinkInventoryError::LinkIdConflict { .. })
    ));

    write_manifest(first_dir.path(), "other-extension", "0.1.0");
    assert!(matches!(
        registry
            .link_package(load_extension_package(first_dir.path()).expect("third"))
            .expect_err("path conflict"),
        ExtensionRegistryError::LinkInventory(LinkInventoryError::LinkPathConflict { .. })
    ));

    assert!(registry
        .link_package(load_extension_package(third_dir.path()).expect("other"))
        .is_ok());
}

#[test]
fn registry_installs_metadata_snapshot_and_uninstalls_it() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    let manifest = fs::read(package_dir.path().join(EXTENSION_MANIFEST_FILE)).expect("manifest");
    let installed = registry
        .install_package(load_extension_package(package_dir.path()).expect("package"))
        .expect("install");

    assert_eq!(installed.id, "example-extension");
    assert_eq!(
        installed.materialization,
        ExtensionMaterialization::Installed
    );
    assert_eq!(installed.status, LinkedExtensionStatus::InstalledInert);
    assert!(installed
        .source_path
        .starts_with(registry.home().extensions_dir().join("installed")));
    assert_ne!(installed.source_path, package_dir.path());
    assert_eq!(
        fs::read(installed.source_path.join(EXTENSION_MANIFEST_FILE)).expect("snapshot"),
        manifest
    );

    fs::remove_file(package_dir.path().join(EXTENSION_MANIFEST_FILE)).expect("remove source");
    assert!(registry
        .linked_extension("example-extension")
        .expect("lookup")
        .is_some());

    let uninstalled = registry
        .uninstall_installed("example-extension")
        .expect("uninstall");
    assert_eq!(
        uninstalled.materialization,
        ExtensionMaterialization::Installed
    );
    assert!(!installed.source_path.exists());
    assert!(registry
        .linked_extension("example-extension")
        .expect("lookup gone")
        .is_none());
    assert!(matches!(
        registry
            .uninstall_installed("example-extension")
            .expect_err("unknown uninstall"),
        ExtensionRegistryError::UnknownInstalledExtension { .. }
    ));
}

#[test]
fn registry_rejects_linked_and_installed_mode_conflicts() {
    let (_temp, linked_registry) = registry();
    let linked_dir = extension_dir("example-extension");
    linked_registry
        .link_package(load_extension_package(linked_dir.path()).expect("linked"))
        .expect("link");
    assert!(matches!(
        linked_registry
            .install_package(load_extension_package(linked_dir.path()).expect("install linked id"))
            .expect_err("install over linked"),
        ExtensionRegistryError::LinkInventory(LinkInventoryError::ModeConflict { .. })
    ));
    assert!(matches!(
        linked_registry
            .uninstall_installed("example-extension")
            .expect_err("uninstall linked"),
        ExtensionRegistryError::WrongExtensionMode { .. }
    ));
    assert!(linked_dir.path().exists());

    let (_temp, registry) = registry();
    let installed_dir = extension_dir("example-extension");
    registry
        .install_package(load_extension_package(installed_dir.path()).expect("installed"))
        .expect("install");
    assert!(matches!(
        registry
            .unlink("example-extension")
            .expect_err("unlink installed"),
        ExtensionRegistryError::WrongExtensionMode { .. }
    ));
    assert!(matches!(
        registry
            .link_package(load_extension_package(installed_dir.path()).expect("link installed id"))
            .expect_err("link over installed"),
        ExtensionRegistryError::LinkInventory(LinkInventoryError::ModeConflict { .. })
    ));
    assert_eq!(
        registry
            .linked_extension("example-extension")
            .expect("lookup")
            .expect("installed")
            .materialization,
        ExtensionMaterialization::Installed
    );
}

#[test]
fn registry_install_is_idempotent_for_same_digest_and_rejects_digest_drift() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    let first = registry
        .install_package(load_extension_package(package_dir.path()).expect("first"))
        .expect("first install");
    let second = registry
        .install_package(load_extension_package(package_dir.path()).expect("same"))
        .expect("same install");

    assert_eq!(second.source_path, first.source_path);
    assert_eq!(second.manifest_sha256, first.manifest_sha256);

    write_manifest(package_dir.path(), "example-extension", "0.2.0");
    assert!(matches!(
        registry
            .install_package(load_extension_package(package_dir.path()).expect("drift"))
            .expect_err("digest drift"),
        ExtensionRegistryError::LinkInventory(LinkInventoryError::InstallManifestConflict { .. })
    ));
}

#[cfg(unix)]
#[test]
fn registry_install_removes_snapshot_when_inventory_write_fails() {
    let (_temp, registry) = registry();
    let target = tempfile::NamedTempFile::new().expect("target file");
    fs::write(target.path(), b"tmp sentinel\n").expect("target content");
    symlink(target.path(), registry.link_inventory_tmp_path()).expect("inventory tmp symlink");
    let package_dir = extension_dir("example-extension");
    let package = load_extension_package(package_dir.path()).expect("package");
    let installed_dir =
        registry.installed_package_dir(&package.descriptor.id, &package.manifest_sha256);

    let error = registry
        .install_package(package)
        .expect_err("symlinked inventory tmp");

    assert!(matches!(error, ExtensionRegistryError::Io(_)));
    // The manifest snapshot lands before the inventory write; compensation
    // must remove it so the failed install leaves no orphan behind.
    assert!(!installed_dir.exists());
    assert!(registry
        .linked_extension("example-extension")
        .expect("lookup")
        .is_none());
    assert_eq!(
        fs::read(target.path()).expect("target after"),
        b"tmp sentinel\n"
    );
}

#[test]
fn registry_install_self_heals_orphaned_snapshot() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    let manifest = fs::read(package_dir.path().join(EXTENSION_MANIFEST_FILE)).expect("manifest");
    let installed = registry
        .install_package(load_extension_package(package_dir.path()).expect("first"))
        .expect("first install");
    // Simulate the install crash window: snapshot on disk (with drifted
    // bytes), no inventory entry recorded.
    fs::remove_file(registry.link_inventory_path()).expect("drop inventory");
    fs::write(
        installed.source_path.join(EXTENSION_MANIFEST_FILE),
        b"stale bytes",
    )
    .expect("stale manifest");
    let stale_extra = installed.source_path.join("stale-extra");
    fs::write(&stale_extra, b"stale").expect("stale extra file");

    let healed = registry
        .install_package(load_extension_package(package_dir.path()).expect("again"))
        .expect("reinstall over orphan");

    assert_eq!(healed.source_path, installed.source_path);
    assert_eq!(
        fs::read(healed.source_path.join(EXTENSION_MANIFEST_FILE)).expect("snapshot"),
        manifest
    );
    assert!(!stale_extra.exists());
    assert_eq!(
        audit_issue(&registry.audit().expect("audit"), "example-extension"),
        ExtensionAuditIssueCode::Ok
    );
}

#[cfg(unix)]
#[test]
fn registry_uninstall_rejects_symlinked_installed_snapshot() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    let installed = registry
        .install_package(load_extension_package(package_dir.path()).expect("package"))
        .expect("install");
    let outside = tempfile::tempdir().expect("outside dir");
    fs::remove_dir_all(&installed.source_path).expect("remove snapshot");
    symlink(outside.path(), &installed.source_path).expect("snapshot symlink");

    assert!(matches!(
        registry
            .uninstall_installed("example-extension")
            .expect_err("symlink uninstall"),
        ExtensionRegistryError::Io(_)
    ));
    assert!(outside.path().exists());
    assert!(registry
        .linked_extension("example-extension")
        .expect("lookup")
        .is_some());
}

#[cfg(unix)]
#[test]
fn registry_uninstall_rejects_symlinked_installed_root_without_rewriting_inventory() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    let installed = registry
        .install_package(load_extension_package(package_dir.path()).expect("package"))
        .expect("install");
    let inventory_before = fs::read(registry.link_inventory_path()).expect("inventory before");
    let installed_root = registry.installed_extensions_dir();
    fs::rename(
        &installed_root,
        registry.home().extensions_dir().join("installed-backup"),
    )
    .expect("move real installed root");

    let outside = tempfile::tempdir().expect("outside dir");
    let outside_snapshot = outside
        .path()
        .join(&installed.id)
        .join(&installed.manifest_sha256);
    fs::create_dir_all(&outside_snapshot).expect("outside snapshot");
    let sentinel = outside_snapshot.join("sentinel");
    fs::write(&sentinel, b"must survive\n").expect("sentinel");
    symlink(outside.path(), &installed_root).expect("installed root symlink");

    let error = registry
        .uninstall_installed("example-extension")
        .expect_err("symlinked installed root");

    assert!(matches!(error, ExtensionRegistryError::Io(_)));
    assert_eq!(
        fs::read(&sentinel).expect("outside sentinel after rejected uninstall"),
        b"must survive\n"
    );
    assert_eq!(
        fs::read(registry.link_inventory_path()).expect("inventory after"),
        inventory_before
    );
    assert!(registry
        .linked_extension("example-extension")
        .expect("lookup")
        .is_some());
}

#[test]
fn registry_uninstall_rejects_malformed_installed_inventory_path() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    let installed = registry
        .install_package(load_extension_package(package_dir.path()).expect("package"))
        .expect("install");
    let mut inventory: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(registry.link_inventory_path()).expect("read"))
            .expect("inventory json");
    inventory["links"]["example-extension"]["source_path"] = serde_json::Value::String(
        registry
            .home()
            .extensions_dir()
            .join("installed")
            .join("example-extension")
            .join("digest")
            .join("extra")
            .to_string_lossy()
            .into_owned(),
    );
    write_link_inventory(
        &registry,
        &format!(
            "{}\n",
            serde_json::to_string_pretty(&inventory).expect("encode inventory")
        ),
    );

    assert!(matches!(
        registry
            .uninstall_installed("example-extension")
            .expect_err("malformed installed path"),
        ExtensionRegistryError::Io(_)
    ));
    assert!(installed.source_path.exists());
}

#[test]
fn registry_reload_missing_path_marks_link_broken() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    registry
        .link_package(load_extension_package(package_dir.path()).expect("package"))
        .expect("link");
    fs::remove_file(package_dir.path().join(EXTENSION_MANIFEST_FILE)).expect("remove manifest");

    let linked = registry
        .reload_link("example-extension")
        .expect("reload broken");

    assert_eq!(linked.status, LinkedExtensionStatus::Broken);
    assert!(linked
        .broken_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("io failed")));
}

#[test]
fn registry_link_inventory_overwrites_stale_temp_file() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    registry
        .link_package(load_extension_package(package_dir.path()).expect("package"))
        .expect("link");
    let stale_tmp = registry.link_inventory_tmp_path();
    fs::write(&stale_tmp, "stale partial inventory").expect("write stale tmp");

    write_manifest(package_dir.path(), "example-extension", "0.2.0");
    let reloaded = registry.reload_link("example-extension").expect("reload");

    assert_eq!(reloaded.descriptor.version, "0.2.0");
    assert!(
        !stale_tmp.exists(),
        "successful inventory rewrite should replace stale temp file"
    );
}

#[test]
fn registry_link_inventory_is_separate_from_enablement_state_log() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    registry
        .link_package(load_extension_package(package_dir.path()).expect("package"))
        .expect("link");
    write_link_inventory(&registry, "not json\n");

    assert!(matches!(
        registry
            .linked_extensions()
            .expect_err("bad link inventory"),
        ExtensionRegistryError::LinkInventory(LinkInventoryError::Json(_))
    ));
    assert_eq!(
        registry
            .enable("session-export")
            .expect("enable general state"),
        ExtensionEnablement::Enabled
    );
}

#[test]
fn registry_audit_reports_empty_inventory() {
    let (_temp, registry) = registry();

    let report = registry.audit().expect("audit");

    assert_eq!(report.schema_version, 1);
    assert!(report.entries.is_empty());
}

#[test]
fn registry_audit_read_only_open_does_not_create_registry_dir() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = EulerHome::from_root(temp.path().join(".euler")).expect("home");
    let extensions_dir = home.extensions_dir();
    assert!(!extensions_dir.exists());
    let registry = ExtensionRegistry::open_read_only(home);

    let report = registry.audit().expect("audit");

    assert!(report.entries.is_empty());
    assert!(!extensions_dir.exists());
}

#[test]
fn registry_audit_reports_healthy_linked_and_installed_records() {
    let (_temp, registry) = registry();
    let linked_dir = extension_dir("linked-extension");
    registry
        .link_package(load_extension_package(linked_dir.path()).expect("linked"))
        .expect("link");
    let installed_dir = extension_dir("installed-extension");
    registry
        .install_package(load_extension_package(installed_dir.path()).expect("installed"))
        .expect("install");

    let report = registry.audit().expect("audit");

    assert_eq!(report.entries.len(), 2);
    assert_eq!(
        audit_issue(&report, "installed-extension"),
        ExtensionAuditIssueCode::Ok
    );
    assert_eq!(
        audit_issue(&report, "linked-extension"),
        ExtensionAuditIssueCode::Ok
    );
}

#[test]
fn registry_audit_reports_linked_manifest_drift() {
    let (_temp, id_registry) = registry();
    let id_dir = extension_dir("example-extension");
    id_registry
        .link_package(load_extension_package(id_dir.path()).expect("package"))
        .expect("link");
    write_manifest(id_dir.path(), "renamed-extension", "0.1.0");

    assert_eq!(
        audit_issue(&id_registry.audit().expect("id audit"), "example-extension"),
        ExtensionAuditIssueCode::LinkedManifestIdMismatch
    );

    let (_temp, digest_registry) = registry();
    let digest_dir = extension_dir("example-extension");
    digest_registry
        .link_package(load_extension_package(digest_dir.path()).expect("package"))
        .expect("link");
    write_manifest(digest_dir.path(), "example-extension", "0.2.0");

    assert_eq!(
        audit_issue(
            &digest_registry.audit().expect("digest audit"),
            "example-extension"
        ),
        ExtensionAuditIssueCode::LinkedManifestDigestMismatch
    );
}

#[test]
fn registry_audit_reports_linked_source_wrong_type_as_unreadable() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    registry
        .link_package(load_extension_package(package_dir.path()).expect("package"))
        .expect("link");
    let file_path = registry.home().extensions_dir().join("not-a-dir");
    fs::write(&file_path, "not a directory").expect("write file");
    set_inventory_source_path(&registry, "example-extension", &file_path);

    assert_eq!(
        audit_issue(&registry.audit().expect("audit"), "example-extension"),
        ExtensionAuditIssueCode::LinkedSourceUnreadable
    );
}

#[cfg(unix)]
#[test]
fn registry_audit_reports_linked_source_symlink_as_unreadable() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    registry
        .link_package(load_extension_package(package_dir.path()).expect("package"))
        .expect("link");
    let target = extension_dir("renamed-extension");
    let symlink_path = registry.home().extensions_dir().join("linked-symlink");
    symlink(target.path(), &symlink_path).expect("linked source symlink");
    set_inventory_source_path(&registry, "example-extension", &symlink_path);

    assert_eq!(
        audit_issue(&registry.audit().expect("audit"), "example-extension"),
        ExtensionAuditIssueCode::LinkedSourceUnreadable
    );
}

#[test]
fn registry_audit_reports_installed_snapshot_damage() {
    let (_temp, missing_registry) = registry();
    let missing_dir = extension_dir("example-extension");
    let missing = missing_registry
        .install_package(load_extension_package(missing_dir.path()).expect("package"))
        .expect("install");
    fs::remove_file(missing.source_path.join(EXTENSION_MANIFEST_FILE)).expect("remove manifest");

    assert_eq!(
        audit_issue(
            &missing_registry.audit().expect("missing audit"),
            "example-extension"
        ),
        ExtensionAuditIssueCode::InstalledSnapshotMissing
    );

    let (_temp, id_registry) = registry();
    let id_dir = extension_dir("example-extension");
    let id_mismatch = id_registry
        .install_package(load_extension_package(id_dir.path()).expect("package"))
        .expect("install");
    write_manifest(&id_mismatch.source_path, "renamed-extension", "0.1.0");

    assert_eq!(
        audit_issue(&id_registry.audit().expect("id audit"), "example-extension"),
        ExtensionAuditIssueCode::InstalledManifestIdMismatch
    );

    let (_temp, digest_registry) = registry();
    let digest_dir = extension_dir("example-extension");
    let digest = digest_registry
        .install_package(load_extension_package(digest_dir.path()).expect("package"))
        .expect("install");
    write_manifest(&digest.source_path, "example-extension", "0.2.0");

    assert_eq!(
        audit_issue(
            &digest_registry.audit().expect("digest audit"),
            "example-extension"
        ),
        ExtensionAuditIssueCode::InstalledManifestDigestMismatch
    );

    let (_temp, unreadable_registry) = registry();
    let unreadable_dir = extension_dir("example-extension");
    let unreadable = unreadable_registry
        .install_package(load_extension_package(unreadable_dir.path()).expect("package"))
        .expect("install");
    fs::remove_dir_all(&unreadable.source_path).expect("remove snapshot");
    fs::write(&unreadable.source_path, "not a directory").expect("write file in snapshot place");

    assert_eq!(
        audit_issue(
            &unreadable_registry.audit().expect("unreadable audit"),
            "example-extension"
        ),
        ExtensionAuditIssueCode::InstalledSnapshotUnreadable
    );

    let (_temp, invalid_registry) = registry();
    let invalid_dir = extension_dir("example-extension");
    let invalid = invalid_registry
        .install_package(load_extension_package(invalid_dir.path()).expect("package"))
        .expect("install");
    fs::write(
        invalid.source_path.join(EXTENSION_MANIFEST_FILE),
        b"not json",
    )
    .expect("write invalid manifest");

    assert_eq!(
        audit_issue(
            &invalid_registry.audit().expect("invalid audit"),
            "example-extension"
        ),
        ExtensionAuditIssueCode::InstalledManifestInvalid
    );

    let (_temp, invalid_path_registry) = registry();
    let invalid_path_dir = extension_dir("example-extension");
    invalid_path_registry
        .install_package(load_extension_package(invalid_path_dir.path()).expect("package"))
        .expect("install");
    let invalid_path = invalid_path_registry
        .home()
        .extensions_dir()
        .join("installed")
        .join("example-extension")
        .join("digest")
        .join("extra");
    set_inventory_source_path(&invalid_path_registry, "example-extension", &invalid_path);

    assert_eq!(
        audit_issue(
            &invalid_path_registry.audit().expect("invalid path audit"),
            "example-extension"
        ),
        ExtensionAuditIssueCode::InstalledPathInvalid
    );
}

#[test]
fn registry_audit_reports_orphaned_installed_snapshot() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    let installed = registry
        .install_package(load_extension_package(package_dir.path()).expect("package"))
        .expect("install");
    // Simulate the uninstall crash window: inventory entry gone, snapshot
    // dir still on disk.
    fs::remove_file(registry.link_inventory_path()).expect("drop inventory");

    let report = registry.audit().expect("audit");

    assert_eq!(report.entries.len(), 1);
    let entry = &report.entries[0];
    assert_eq!(entry.id, "example-extension");
    assert_eq!(entry.source_kind, "installed");
    assert_eq!(entry.recorded_status, "unrecorded");
    assert_eq!(
        entry.issue_code,
        ExtensionAuditIssueCode::InstalledSnapshotOrphaned
    );
    // Audit only reports the orphan; the dir itself is left in place.
    assert!(installed.source_path.exists());
}

#[cfg(unix)]
#[test]
fn registry_audit_reports_installed_snapshot_symlink_without_removal() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    let installed = registry
        .install_package(load_extension_package(package_dir.path()).expect("package"))
        .expect("install");
    let outside = tempfile::tempdir().expect("outside dir");
    fs::remove_dir_all(&installed.source_path).expect("remove snapshot");
    symlink(outside.path(), &installed.source_path).expect("snapshot symlink");

    assert_eq!(
        audit_issue(&registry.audit().expect("audit"), "example-extension"),
        ExtensionAuditIssueCode::InstalledSnapshotSymlink
    );
    assert!(outside.path().exists());
}

#[cfg(unix)]
#[test]
fn registry_audit_reports_installed_parent_symlink_as_path_invalid() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    let installed = registry
        .install_package(load_extension_package(package_dir.path()).expect("package"))
        .expect("install");
    let parent = installed.source_path.parent().expect("installed parent");
    let outside = tempfile::tempdir().expect("outside dir");
    fs::remove_dir_all(parent).expect("remove installed id dir");
    symlink(outside.path(), parent).expect("installed parent symlink");

    assert_eq!(
        audit_issue(&registry.audit().expect("audit"), "example-extension"),
        ExtensionAuditIssueCode::InstalledPathInvalid
    );
    assert!(outside.path().exists());
}

#[cfg(unix)]
#[test]
fn registry_link_state_uses_private_permissions() {
    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    registry
        .link_package(load_extension_package(package_dir.path()).expect("package"))
        .expect("link");
    let _lock = registry.acquire_link_lock().expect("lock");

    assert_eq!(mode(&registry.link_inventory_path()), 0o600);
    assert_eq!(mode(&registry.link_inventory_lock_path()), 0o600);
}

fn registry() -> (tempfile::TempDir, ExtensionRegistry) {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = EulerHome::from_root(temp.path().join(".euler")).expect("home");
    let registry = ExtensionRegistry::new(home).expect("registry");
    (temp, registry)
}

fn write_state_log(registry: &ExtensionRegistry, content: &str) {
    fs::write(registry.state_log_path(), content).expect("write state log");
}

fn write_link_inventory(registry: &ExtensionRegistry, content: &str) {
    fs::write(registry.link_inventory_path(), content).expect("write link inventory");
}

fn set_inventory_source_path(registry: &ExtensionRegistry, id: &str, path: &std::path::Path) {
    let mut inventory: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(registry.link_inventory_path()).expect("read"))
            .expect("inventory json");
    inventory["links"][id]["source_path"] =
        serde_json::Value::String(path.to_string_lossy().into_owned());
    write_link_inventory(
        registry,
        &format!(
            "{}\n",
            serde_json::to_string_pretty(&inventory).expect("encode inventory")
        ),
    );
}

fn audit_issue(report: &ExtensionAuditReport, id: &str) -> ExtensionAuditIssueCode {
    report
        .entries
        .iter()
        .find(|entry| entry.id == id)
        .unwrap_or_else(|| panic!("missing audit entry for {id}"))
        .issue_code
}

fn extension_dir(id: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("extension dir");
    write_manifest(dir.path(), id, "0.1.0");
    dir
}

fn managed_process_extension_dir(id: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("managed-process extension dir");
    write_managed_process_manifest(dir.path(), id, "0.1.0");
    dir
}

fn write_manifest(dir: &std::path::Path, id: &str, version: &str) {
    fs::write(
        dir.join(EXTENSION_MANIFEST_FILE),
        format!(
            r#"{{
  "version": 1,
  "id": "{id}",
  "display_name": "Example Extension",
  "extension_version": "{version}",
  "runtime_kind": "native-rust",
  "capabilities": ["provenance-read"],
  "commands": [
    {{
      "name": "inspect",
      "display_name": "Inspect",
      "summary": "Inspect provenance.",
      "required_capabilities": ["provenance-read"]
    }}
  ]
}}"#
        ),
    )
    .expect("write manifest");
}

fn write_managed_process_manifest(dir: &std::path::Path, id: &str, version: &str) {
    fs::write(
        dir.join(EXTENSION_MANIFEST_FILE),
        format!(
            r#"{{
  "version": 1,
  "id": "{id}",
  "display_name": "Managed Process Extension",
  "extension_version": "{version}",
  "runtime_kind": "managed-process",
  "entrypoint": {{"command": ["python3", "-u", "extension.py"]}},
  "capabilities": ["provenance-read"],
  "commands": [
    {{
      "name": "inspect",
      "display_name": "Inspect",
      "summary": "Inspect provenance.",
      "required_capabilities": ["provenance-read"]
    }}
  ]
}}"#
        ),
    )
    .expect("write managed-process manifest");
}

#[cfg(unix)]
fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[test]
fn registry_install_removes_snapshot_when_inventory_sync_fault_is_injected() {
    use crate::durability::fault::{arm_matching, Op};

    let (_temp, registry) = registry();
    let package_dir = extension_dir("example-extension");
    let package = load_extension_package(package_dir.path()).expect("package");
    let installed_dir =
        registry.installed_package_dir(&package.descriptor.id, &package.manifest_sha256);

    // Fail the inventory temp-file sync (a pre-rename failure, like the
    // symlinked-tmp variant above, but injected directly at the seam).
    let inventory_tmp = registry.link_inventory_tmp_path();
    let guard = arm_matching(Op::FileSync, move |path| path == inventory_tmp);
    let error = registry
        .install_package(package)
        .expect_err("injected inventory sync failure");
    assert!(guard.fired());
    drop(guard);

    assert!(matches!(error, ExtensionRegistryError::Io(_)));
    // The manifest snapshot lands before the inventory write; compensation
    // must remove it so the failed install leaves no orphan behind.
    assert!(!installed_dir.exists());
    assert!(registry
        .linked_extension("example-extension")
        .expect("lookup")
        .is_none());
}
