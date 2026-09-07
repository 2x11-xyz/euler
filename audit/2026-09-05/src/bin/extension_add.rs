use euler_sdk::extension_package::{apply_install_package, apply_link_package};
use euler_sdk::{LoadedExtensionPackage, StaticExtensionDescriptor};
use std::collections::BTreeMap;
fn main() {
    let fixture = tempfile::tempdir().unwrap();
    let p = LoadedExtensionPackage {
        canonical_dir: fixture.path().join("synthetic-package"),
        manifest_bytes: vec![],
        manifest_sha256: "0".repeat(64),
        descriptor: StaticExtensionDescriptor {
            id: "audit-add".into(),
            display_name: "Audit".into(),
            version: "0.0.0".into(),
            runtime_kind: "managed-process".into(),
            capabilities: vec![],
            commands: vec![],
            observer: None,
            idle_contribution: None,
        },
    };
    let mut links = BTreeMap::new();
    println!(
        "link={:?}",
        apply_link_package(&mut links, p.clone()).map(|l| l.materialization)
    );
    println!(
        "install={:?}",
        apply_install_package(&mut links, p, fixture.path().join("not-created"))
    );
    println!(
        "remaining_materialization={:?}",
        links["audit-add"].materialization
    );
}
