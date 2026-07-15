use super::*;
use serde_json::json;
use std::collections::BTreeMap;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

#[test]
fn capability_validation_matches_canonical_parser() {
    let cases = [
        "fs-read",
        "fs-write",
        "provenance-read",
        "diagnostics-read",
        "artifact-write",
        "agent-record",
        "shell-exec",
        "network",
        "config-write",
        "secret-resolve",
        "Fs-Read",
        "fs-Read",
        " fs-read",
        "fs-read ",
        "",
        "unknown",
        "network-read",
    ];

    for case in cases {
        assert_eq!(valid_capability(case), Capability::parse(case).is_some());
    }
}

#[test]
fn parses_valid_manifest_and_hashes_raw_bytes() {
    let temp = tempfile::tempdir().expect("temp dir");
    let manifest = valid_manifest("example-extension");
    fs::write(
        temp.path().join(EXTENSION_MANIFEST_FILE),
        manifest.as_bytes(),
    )
    .expect("write manifest");

    let package = load_extension_package(temp.path()).expect("package");

    assert_eq!(package.descriptor.id, "example-extension");
    assert_eq!(package.descriptor.version, "0.1.0");
    assert_eq!(package.descriptor.runtime_kind, "native-rust");
    assert_eq!(package.descriptor.commands[0].name, "inspect");
    assert_eq!(package.manifest_bytes, manifest.as_bytes());
    assert_eq!(
        package.manifest_sha256,
        manifest_sha256_hex(manifest.as_bytes())
    );
    assert!(package.canonical_dir.is_absolute());
}

#[test]
fn parses_agent_record_capability_in_manifest_and_command() {
    let manifest = valid_manifest("agent-extension").replace(
        r#""capabilities": ["provenance-read"]"#,
        r#""capabilities": ["provenance-read", "agent-record"]"#,
    );
    let manifest = manifest.replace(
        r#""required_capabilities": ["provenance-read"]"#,
        r#""required_capabilities": ["agent-record"]"#,
    );

    let parsed = parse_extension_manifest_bytes(manifest.as_bytes()).expect("manifest");

    assert_eq!(
        parsed.capabilities,
        vec!["provenance-read".to_owned(), "agent-record".to_owned()]
    );
    assert_eq!(
        parsed.commands[0].required_capabilities,
        vec!["agent-record".to_owned()]
    );
}

#[test]
fn parses_managed_process_entrypoint_and_rejects_ambiguous_runtime_shapes() {
    let manifest = valid_manifest("process-extension").replace(
        r#""runtime_kind": "native-rust","#,
        r#""runtime_kind": "managed-process",
      "entrypoint": {"command": ["python3", "-u", "extension.py"]},"#,
    );
    let parsed = parse_extension_manifest_bytes(manifest.as_bytes()).expect("managed manifest");
    assert_eq!(parsed.runtime_kind, "managed-process");
    assert_eq!(
        managed_process_entrypoint_from_manifest_bytes(manifest.as_bytes()).expect("entrypoint"),
        ManagedProcessEntrypoint {
            command: vec![
                "python3".to_owned(),
                "-u".to_owned(),
                "extension.py".to_owned()
            ]
        }
    );

    let missing_entrypoint = valid_manifest("process-extension").replace(
        r#""runtime_kind": "native-rust""#,
        r#""runtime_kind": "managed-process""#,
    );
    assert!(
        parse_extension_manifest_bytes(missing_entrypoint.as_bytes())
            .expect_err("entrypoint is required")
            .to_string()
            .contains("requires entrypoint")
    );

    let native_with_entrypoint = valid_manifest("native-extension").replace(
        r#""capabilities": ["provenance-read"],"#,
        r#""entrypoint": {"command": ["python3", "extension.py"]},
      "capabilities": ["provenance-read"],"#,
    );
    assert!(
        parse_extension_manifest_bytes(native_with_entrypoint.as_bytes())
            .expect_err("native entrypoint")
            .to_string()
            .contains("only valid for runtime_kind managed-process")
    );

    let control_character = manifest.replace("extension.py", "bad\\nscript.py");
    assert!(parse_extension_manifest_bytes(control_character.as_bytes())
        .expect_err("control character")
        .to_string()
        .contains("must not contain control characters"));

    let mut empty_command: serde_json::Value =
        serde_json::from_str(&manifest).expect("managed manifest json");
    empty_command["entrypoint"]["command"] = json!([]);
    assert!(parse_extension_manifest_bytes(
        &serde_json::to_vec(&empty_command).expect("empty command manifest")
    )
    .expect_err("empty argv")
    .to_string()
    .contains("must not be empty"));

    let mut too_many_args: serde_json::Value =
        serde_json::from_str(&manifest).expect("managed manifest json");
    too_many_args["entrypoint"]["command"] = serde_json::Value::Array(
        (0..=MAX_MANAGED_PROCESS_ENTRYPOINT_ARGS)
            .map(|_| json!("python3"))
            .collect(),
    );
    assert!(parse_extension_manifest_bytes(
        &serde_json::to_vec(&too_many_args).expect("too many args manifest")
    )
    .expect_err("argv count limit")
    .to_string()
    .contains("maximum is"));

    let mut oversized_arg: serde_json::Value =
        serde_json::from_str(&manifest).expect("managed manifest json");
    oversized_arg["entrypoint"]["command"] = json!([
        "python3",
        "x".repeat(MAX_MANAGED_PROCESS_ENTRYPOINT_ARG_BYTES + 1),
    ]);
    assert!(parse_extension_manifest_bytes(
        &serde_json::to_vec(&oversized_arg).expect("oversized arg manifest")
    )
    .expect_err("argv item limit")
    .to_string()
    .contains("is too long"));
}

#[test]
fn rejects_unknown_and_secret_like_fields_without_values() {
    let secret_manifest = r#"{
      "version": 1,
      "id": "example-extension",
      "display_name": "Example",
      "extension_version": "0.1.0",
      "runtime_kind": "native-rust",
      "capabilities": ["provenance-read"],
      "commands": [
        {
          "name": "inspect",
          "display_name": "Inspect",
          "summary": "Inspect provenance.",
          "required_capabilities": ["provenance-read"],
          "api_key": "SHOULD_NOT_APPEAR"
        }
      ]
    }"#;

    let error = parse_extension_manifest_bytes(secret_manifest.as_bytes())
        .expect_err("secret field rejected")
        .to_string();

    assert!(error.contains("forbidden secret-like field"));
    assert!(error.contains("api_key"));
    assert!(!error.contains("SHOULD_NOT_APPEAR"));

    let unknown = valid_manifest("example-extension")
        .replace(r#""commands":"#, r#""extra": "ignored", "commands":"#);
    let error = parse_extension_manifest_bytes(unknown.as_bytes())
        .expect_err("unknown field rejected")
        .to_string();
    assert!(error.contains("unknown field `manifest.extra`"));
}

#[test]
fn rejects_invalid_identifiers_duplicates_and_capability_drift() {
    let invalid_id = valid_manifest("Bad");
    assert!(parse_extension_manifest_bytes(invalid_id.as_bytes())
        .expect_err("bad id")
        .to_string()
        .contains("manifest id is not a valid extension identifier"));

    let duplicate_command = valid_manifest("example-extension").replace(
        r#"{
          "name": "inspect","#,
        r#"{
          "name": "inspect-2","#,
    );
    let duplicate_command = duplicate_command.replace(
        r#"}
      ]
    }"#,
        r#"},
        {
          "name": "inspect-2",
          "display_name": "Inspect",
          "summary": "Inspect provenance.",
          "required_capabilities": ["provenance-read"]
        }
      ]
    }"#,
    );
    assert!(parse_extension_manifest_bytes(duplicate_command.as_bytes())
        .expect_err("duplicate command")
        .to_string()
        .contains("duplicate command name"));

    let duplicate_capability = valid_manifest("example-extension").replace(
        r#""capabilities": ["provenance-read"]"#,
        r#""capabilities": ["provenance-read", "provenance-read"]"#,
    );
    assert!(
        parse_extension_manifest_bytes(duplicate_capability.as_bytes())
            .expect_err("duplicate capability")
            .to_string()
            .contains("duplicate capability")
    );

    let drift = valid_manifest("example-extension").replace(
        r#""required_capabilities": ["provenance-read"]"#,
        r#""required_capabilities": ["fs-read"]"#,
    );
    assert!(parse_extension_manifest_bytes(drift.as_bytes())
        .expect_err("capability outside envelope")
        .to_string()
        .contains("outside manifest envelope"));
}

#[test]
fn rejects_oversized_manifest_and_non_directory_path() {
    let too_large = vec![b' '; (MAX_EXTENSION_MANIFEST_BYTES + 1) as usize];
    assert!(matches!(
        parse_extension_manifest_bytes(&too_large),
        Err(ExtensionPackageError::ManifestTooLarge { .. })
    ));

    let temp = tempfile::tempdir().expect("temp dir");
    let file = temp.path().join("not-a-dir");
    fs::write(&file, "").expect("write file");

    assert!(matches!(
        load_extension_package(&file),
        Err(ExtensionPackageError::NotDirectory { .. })
    ));
}

#[test]
fn load_does_not_execute_files_in_extension_directory() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(
        temp.path().join(EXTENSION_MANIFEST_FILE),
        valid_manifest("example-extension"),
    )
    .expect("write manifest");
    let sentinel = temp.path().join("sentinel-created");
    fs::write(
        temp.path().join("build.sh"),
        format!("#!/bin/sh\ntouch {}\n", sentinel.display()),
    )
    .expect("write script");

    load_extension_package(temp.path()).expect("package");

    assert!(
        !sentinel.exists(),
        "manifest loading must not execute scripts"
    );
}

#[test]
fn link_inventory_round_trips_status_and_rejects_bad_schema() {
    let mut links = BTreeMap::new();
    links.insert(
        "example-extension".to_owned(),
        linked_extension(
            "example-extension",
            LinkedExtensionStatus::Broken,
            Some("manifest missing".to_owned()),
        ),
    );

    let encoded = encode_link_inventory(&links).expect("encode");
    let text = std::str::from_utf8(&encoded).expect("utf8");
    assert!(text.contains(r#""materialization": "linked""#));
    assert!(text.contains(r#""status": "broken""#));
    assert!(text.contains(r#""broken_reason": "manifest missing""#));

    let decoded = decode_link_inventory(text).expect("decode");
    let linked = decoded.get("example-extension").expect("linked");
    assert_eq!(linked.status, LinkedExtensionStatus::Broken);
    assert_eq!(linked.broken_reason.as_deref(), Some("manifest missing"));
    assert_eq!(linked.updated_ts_ms, 7);

    let bad_version = text.replacen(r#""v": 1"#, r#""v": 99"#, 1);
    assert!(matches!(
        decode_link_inventory(&bad_version),
        Err(LinkInventoryError::UnsupportedVersion { version: 99 })
    ));

    let unknown = text.replacen(r#""links":"#, r#""unknown": 1, "links":"#, 1);
    assert!(matches!(
        decode_link_inventory(&unknown),
        Err(LinkInventoryError::Json(_))
    ));
}

#[test]
fn link_inventory_keeps_the_existing_v1_wire_format() {
    let mut links = BTreeMap::new();
    links.insert(
        "example-extension".to_owned(),
        linked_extension(
            "example-extension",
            LinkedExtensionStatus::NeedsReview,
            None,
        ),
    );
    let encoded = String::from_utf8(encode_link_inventory(&links).expect("encode inventory"))
        .expect("inventory utf8");
    assert!(encoded.contains(r#""v": 1"#));
    let decoded = decode_link_inventory(&encoded).expect("decode v1 inventory");
    assert_eq!(
        decoded
            .get("example-extension")
            .expect("legacy package")
            .status,
        LinkedExtensionStatus::NeedsReview
    );
}

#[test]
fn link_inventory_defaults_missing_materialization_to_linked() {
    let inventory = json!({
        "v": LINK_INVENTORY_VERSION,
        "links": {
            "example-extension": inventory_record("example-extension", "/tmp/example-extension")
        }
    })
    .to_string();

    let decoded = decode_link_inventory(&inventory).expect("decode");
    let linked = decoded.get("example-extension").expect("linked");
    assert_eq!(linked.materialization, ExtensionMaterialization::Linked);
}

#[test]
fn link_inventory_encode_rejects_inconsistent_ids() {
    let mut links = BTreeMap::new();
    let mut linked = linked_extension(
        "example-extension",
        LinkedExtensionStatus::NeedsReview,
        None,
    );
    linked.descriptor.id = "other-extension".to_owned();
    links.insert("example-extension".to_owned(), linked);

    assert!(matches!(
        encode_link_inventory(&links),
        Err(LinkInventoryError::InvalidExtensionId { id }) if id == "other-extension"
    ));
}

#[test]
fn link_inventory_rejects_duplicate_source_paths() {
    let mut links = BTreeMap::new();
    let first = linked_extension(
        "example-extension",
        LinkedExtensionStatus::NeedsReview,
        None,
    );
    let mut second = linked_extension("other-extension", LinkedExtensionStatus::NeedsReview, None);
    second.source_path = first.source_path.clone();
    links.insert("example-extension".to_owned(), first);
    links.insert("other-extension".to_owned(), second);

    assert!(matches!(
        encode_link_inventory(&links),
        Err(LinkInventoryError::LinkPathConflict { .. })
    ));

    let duplicate = json!({
        "v": LINK_INVENTORY_VERSION,
        "links": {
            "example-extension": inventory_record("example-extension", "/tmp/shared-extension"),
            "other-extension": inventory_record("other-extension", "/tmp/shared-extension")
        }
    })
    .to_string();
    assert!(matches!(
        decode_link_inventory(&duplicate),
        Err(LinkInventoryError::LinkPathConflict { .. })
    ));
}

#[cfg(unix)]
#[test]
fn link_inventory_encode_rejects_non_utf8_source_path() {
    let mut links = BTreeMap::new();
    let mut linked = linked_extension(
        "example-extension",
        LinkedExtensionStatus::NeedsReview,
        None,
    );
    linked.source_path = PathBuf::from(OsString::from_vec(vec![b'/', b't', b'm', b'p', 0xff]));
    links.insert("example-extension".to_owned(), linked);

    assert!(matches!(
        encode_link_inventory(&links),
        Err(LinkInventoryError::NonUtf8LinkPath)
    ));
}

fn valid_manifest(id: &str) -> String {
    format!(
        r#"{{
      "version": 1,
      "id": "{id}",
      "display_name": "Example Extension",
      "extension_version": "0.1.0",
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
    )
}

fn linked_extension(
    id: &str,
    status: LinkedExtensionStatus,
    broken_reason: Option<String>,
) -> LinkedExtension {
    LinkedExtension {
        id: id.to_owned(),
        materialization: ExtensionMaterialization::Linked,
        source_path: PathBuf::from(format!("/tmp/{id}")),
        manifest_sha256: "abc123".to_owned(),
        updated_ts_ms: 7,
        status,
        descriptor: StaticExtensionDescriptor {
            id: id.to_owned(),
            display_name: "Example Extension".to_owned(),
            version: "0.1.0".to_owned(),
            runtime_kind: "native-rust".to_owned(),
            capabilities: vec!["provenance-read".to_owned()],
            commands: vec![StaticCommandDescriptor {
                invocation: crate::Invocation::User,
                name: "inspect".to_owned(),
                display_name: "Inspect".to_owned(),
                summary: "Inspect provenance.".to_owned(),
                required_capabilities: vec!["provenance-read".to_owned()],
            }],
        },
        broken_reason,
    }
}

fn inventory_record(id: &str, source_path: &str) -> serde_json::Value {
    json!({
        "source_path": source_path,
        "manifest_sha256": "abc123",
        "updated_ts_ms": 7,
        "status": "needs-review",
        "broken_reason": null,
        "descriptor": {
            "id": id,
            "display_name": "Example Extension",
            "version": "0.1.0",
            "runtime_kind": "native-rust",
            "capabilities": ["provenance-read"],
            "commands": [{
                "name": "inspect",
                "display_name": "Inspect",
                "summary": "Inspect provenance.",
                "required_capabilities": ["provenance-read"]
            }]
        }
    })
}
