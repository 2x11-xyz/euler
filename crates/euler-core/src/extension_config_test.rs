use super::*;
use crate::durability::fault;
use serde_json::json;

fn config_error(result: Result<impl std::fmt::Debug, ExtensionError>) -> String {
    match result.expect_err("config error") {
        ExtensionError::ConfigFailed(message) => message,
        other => panic!("expected ConfigFailed, got {other:?}"),
    }
}

fn temp_files(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .expect("read dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.ends_with(".tmp"))
        .collect()
}

#[test]
fn documents_live_at_the_adr_paths() {
    assert_eq!(
        project_config_path(Path::new("/work"), "demo"),
        Path::new("/work/.euler/extensions/demo/config.json")
    );
    assert_eq!(
        user_config_path(Path::new("/home/u/.euler/extensions"), "demo"),
        Path::new("/home/u/.euler/extensions/demo/config.json")
    );
}

#[test]
fn missing_document_reads_as_none() {
    let temp = tempfile::tempdir().expect("temp");
    assert_eq!(
        read_document(temp.path(), &temp.path().join("demo/config.json")).expect("read"),
        None
    );
}

#[test]
fn malformed_document_is_an_error_not_an_absent_one() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("config.json");
    for (bytes, expected) in [
        (&b"{not json"[..], "config document is malformed JSON"),
        (&b"[\"a\"]"[..], "config document is not a JSON object"),
        (&b"\xff\xfe"[..], "config document is malformed JSON"),
    ] {
        fs::write(&path, bytes).expect("write");
        assert_eq!(config_error(read_document(temp.path(), &path)), expected);
    }
}

#[test]
fn oversize_document_is_an_error() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("config.json");
    let filler = "x".repeat(MAX_EXTENSION_CONFIG_BYTES);
    fs::write(&path, format!("{{\"a\":\"{filler}\"}}")).expect("write");
    assert_eq!(
        config_error(read_document(temp.path(), &path)),
        "config document exceeds 16 KiB"
    );
}

#[test]
fn encode_accepts_only_bounded_json_objects() {
    let redactor = SecretRedactor::new();
    let bytes = encode_document(&json!({"targets": ["a::b"]}), &redactor).expect("encode");
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).expect("json"),
        json!({"targets": ["a::b"]})
    );
    assert!(bytes.len() <= MAX_EXTENSION_CONFIG_BYTES);

    for value in [json!(["a"]), json!("text"), json!(null), json!(3)] {
        assert_eq!(
            config_error(encode_document(&value, &redactor)),
            "config value must be a JSON object"
        );
    }
    let oversize = json!({"a": "x".repeat(MAX_EXTENSION_CONFIG_BYTES)});
    assert_eq!(
        config_error(encode_document(&oversize, &redactor)),
        "config value exceeds 16 KiB"
    );
}

#[test]
fn encode_refuses_values_matching_a_known_secret() {
    let redactor = SecretRedactor::new();
    redactor.add_value("registered-config-secret-0123456789");
    for value in [
        json!({"token": "registered-config-secret-0123456789"}),
        json!({"nested": [{"registered-config-secret-0123456789": true}]}),
        json!({"key": "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789"}),
    ] {
        assert_eq!(
            config_error(encode_document(&value, &redactor)),
            "config value contains a known secret"
        );
    }
}

#[test]
fn write_replaces_the_document_and_leaves_no_temp_file() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("demo/config.json");
    write_document(temp.path(), &path, b"{\"v\":1}\n", false).expect("first write");
    write_document(temp.path(), &path, b"{\"v\":2}\n", false).expect("second write");
    assert_eq!(
        read_document(temp.path(), &path).expect("read"),
        json!({"v": 2}).as_object().cloned()
    );
    assert!(temp_files(path.parent().expect("dir")).is_empty());
}

#[cfg(unix)]
#[test]
fn user_tier_write_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("demo/config.json");
    write_document(temp.path(), &path, b"{}\n", true).expect("write");
    let mode = fs::metadata(&path).expect("metadata").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn failed_write_keeps_the_previous_document_intact() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("demo/config.json");
    write_document(temp.path(), &path, b"{\"v\":1}\n", false).expect("first write");
    let guard = fault::arm_matching(fault::Op::FileSync, |path| {
        path.file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".tmp"))
    });

    let error = config_error(write_document(temp.path(), &path, b"{\"v\":2}\n", false));

    assert!(guard.fired(), "the temp-file sync fault must fire");
    assert!(error.starts_with("config document io failed"), "{error}");
    assert_eq!(fs::read(&path).expect("read"), b"{\"v\":1}\n");
    assert!(temp_files(path.parent().expect("dir")).is_empty());
}

#[cfg(unix)]
#[test]
fn symlinked_config_directory_or_document_is_refused() {
    let temp = tempfile::tempdir().expect("temp");
    let elsewhere = temp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("elsewhere");
    fs::write(elsewhere.join("config.json"), "{}").expect("target");
    let linked_dir = temp.path().join("linked");
    std::os::unix::fs::symlink(&elsewhere, &linked_dir).expect("dir link");
    let path = linked_dir.join("config.json");
    assert_eq!(
        config_error(read_document(temp.path(), &path)),
        "config directory must not be a symlink"
    );
    assert_eq!(
        config_error(write_document(temp.path(), &path, b"{}\n", false)),
        "config directory must not be a symlink"
    );

    // An ancestor below the root, such as a committed `.euler` symlink, is
    // refused too, and nothing is created through it.
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    std::os::unix::fs::symlink(&elsewhere, workspace.join(".euler")).expect("ancestor link");
    let nested = project_config_path(&workspace, "demo");
    assert_eq!(
        config_error(write_document(&workspace, &nested, b"{}\n", false)),
        "config directory must not be a symlink"
    );
    assert_eq!(
        config_error(read_document(&workspace, &nested)),
        "config directory must not be a symlink"
    );
    assert!(!elsewhere.join("extensions").exists());

    let real_dir = temp.path().join("real");
    fs::create_dir_all(&real_dir).expect("real");
    let linked_file = real_dir.join("config.json");
    std::os::unix::fs::symlink(elsewhere.join("config.json"), &linked_file).expect("file link");
    assert_eq!(
        config_error(read_document(temp.path(), &linked_file)),
        "config document is not a regular file"
    );
    assert_eq!(
        config_error(write_document(temp.path(), &linked_file, b"{}\n", false)),
        "config document is not a regular file"
    );
}
