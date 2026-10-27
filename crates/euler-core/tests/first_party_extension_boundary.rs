//! Boundary guard (ADR 0022 Decision 3, boundary contract): core crates name
//! no first-party extension. The scan reads every non-test Rust source in
//! the guarded crates, doc comments included, and fails on any first-party
//! extension id in its hyphen, underscore, or joined spelling. Test files are
//! excluded by name; an inline `#[cfg(test)]` module inside a production file
//! is scanned like the rest of that file.

use std::fs;
use std::path::{Path, PathBuf};

/// Crates that must stay product-neutral. The CLI is not guarded yet: its
/// exit recap still names `session-export`.
const GUARDED_CRATES: &[&str] = &["euler-core", "euler-sdk", "euler-agents", "euler-event"];

/// First-party extension ids as words, so each id is checked in every
/// spelling (`code-swarm`, `code_swarm`, `codeswarm`), case-insensitively.
const FIRST_PARTY_IDS: &[&[&str]] = &[
    &["code", "swarm"],
    &["causal", "dag"],
    &["autoresearch"],
    &["maxproof"],
    &["session", "export"],
    &["plan", "todo"],
    &["diagnostics", "report"],
];

#[test]
fn guarded_crates_name_no_first_party_extension() {
    let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory");
    let mut scanned = 0;
    let mut violations = Vec::new();
    for name in GUARDED_CRATES {
        let src = crates_dir.join(name).join("src");
        for file in production_sources(&src) {
            scanned += 1;
            violations.extend(first_party_mentions(&file));
        }
    }
    assert!(
        scanned > 0,
        "the scan found no sources under {crates_dir:?}"
    );
    assert!(
        violations.is_empty(),
        "core crates must not name first-party extensions (ADR 0022):\n{}",
        violations.join("\n")
    );
}

#[test]
fn boundary_scan_catches_a_planted_id_and_skips_test_files() {
    let temp = tempfile::tempdir().expect("temp dir");
    let src = temp.path().join("src");
    fs::create_dir_all(src.join("nested")).expect("nested dir");
    fs::create_dir_all(src.join("tests")).expect("tests dir");
    fs::write(src.join("lib.rs"), "pub mod nested;\n").expect("lib");
    fs::write(
        src.join("nested").join("planted.rs"),
        "/// Wires the Session_Export host.\npub fn wire() {}\n",
    )
    .expect("planted");
    fs::write(src.join("lib_test.rs"), "// code-swarm\n").expect("unit test file");
    fs::write(src.join("nested").join("tests.rs"), "// causal_dag\n").expect("tests module");
    fs::write(src.join("tests").join("fixture.rs"), "// maxproof\n").expect("tests dir file");

    let files = production_sources(&src);
    assert_eq!(files.len(), 2, "test files are excluded: {files:?}");
    let violations = files
        .iter()
        .flat_map(|file| first_party_mentions(file))
        .collect::<Vec<_>>();
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(violations[0].contains("planted.rs:1"), "{violations:?}");
    assert!(violations[0].contains("session_export"), "{violations:?}");
}

/// Rust sources under `src`, excluding test files: `*_test.rs`,
/// `*_tests.rs`, `tests.rs`, and anything under a `tests` directory.
fn production_sources(src: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![src.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read source dir") {
            let path = entry.expect("source entry").path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("utf-8 source name");
            if path.is_dir() {
                if name != "tests" {
                    pending.push(path);
                }
                continue;
            }
            let Some(stem) = name.strip_suffix(".rs") else {
                continue;
            };
            if stem == "tests" || stem.ends_with("_test") || stem.ends_with("_tests") {
                continue;
            }
            files.push(path);
        }
    }
    files.sort();
    files
}

/// One `path:line: spelling` entry per line that names a first-party id.
fn first_party_mentions(file: &Path) -> Vec<String> {
    let text = fs::read_to_string(file).expect("read source");
    let mut mentions = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.to_ascii_lowercase();
        for words in FIRST_PARTY_IDS {
            let found = ["-", "_", ""]
                .map(|separator| words.join(separator))
                .into_iter()
                .find(|spelling| line.contains(spelling));
            if let Some(spelling) = found {
                mentions.push(format!("{}:{}: {spelling}", file.display(), index + 1));
            }
        }
    }
    mentions
}
