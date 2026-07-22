use std::fs;
use std::path::{Path, PathBuf};

const BASELINE: usize = 6; // decrement as debt is paid; never increment without an ADR

#[test]
fn production_allow_count_does_not_increase() {
    let root = workspace_root();
    let matches = count_workspace_allows(&root);
    assert!(
        matches.len() <= BASELINE,
        "production allow count {} exceeds baseline {BASELINE}:\n{}",
        matches.len(),
        matches
            .iter()
            .map(AllowMatch::failure_line)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn matcher_counts_real_outer_allow() {
    let matches = find_allows("src/lib.rs", "#[allow(dead_code)]\nfn helper() {}\n", false);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].form, "#[allow(");
}

#[test]
fn matcher_counts_inner_allow() {
    let matches = find_allows("src/lib.rs", "#![allow(unused_imports)]\n", true);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].form, "#![allow(");
}

#[test]
fn matcher_counts_cfg_attr_allow() {
    let matches = find_allows(
        "src/module.rs",
        "#[cfg_attr(feature = \"x\", allow(dead_code))]\n",
        false,
    );
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].form, "#[cfg_attr(..., allow|expect(");
}

#[test]
fn matcher_counts_expect_attr() {
    let matches = find_allows(
        "src/lib.rs",
        "#[expect(dead_code)]
fn helper() {}
",
        false,
    );
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].form, "#[expect(");
}

#[test]
fn matcher_ignores_doc_comment_example() {
    let matches = find_allows("src/lib.rs", "/// #[allow(dead_code)]\n", true);
    assert!(matches.is_empty());
}

#[test]
fn matcher_counts_multiline_attr_from_start_line() {
    let matches = find_allows("src/module.rs", "#[\n  allow(dead_code)\n]\n", false);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].line, 1);
    assert_eq!(matches[0].form, "#[allow(");
}

#[derive(Debug, Eq, PartialEq)]
struct AllowMatch {
    file: String,
    line: usize,
    form: &'static str,
    snippet: String,
}

impl AllowMatch {
    fn failure_line(&self) -> String {
        format!("{}:{} {} {}", self.file, self.line, self.form, self.snippet)
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn count_workspace_allows(root: &Path) -> Vec<AllowMatch> {
    let crates = root.join("crates");
    let mut matches = Vec::new();
    for entry in fs::read_dir(&crates).expect("read crates dir") {
        let entry = entry.expect("crate entry");
        let src = entry.path().join("src");
        collect_source_allows(root, &src, &mut matches);
    }
    matches.sort_by(|left, right| left.file.cmp(&right.file).then(left.line.cmp(&right.line)));
    matches
}

fn collect_source_allows(root: &Path, path: &Path, matches: &mut Vec<AllowMatch>) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        if path.file_name().and_then(|name| name.to_str()) == Some("tests") {
            return;
        }
        for entry in fs::read_dir(path).expect("read source dir") {
            collect_source_allows(root, &entry.expect("source entry").path(), matches);
        }
        return;
    }
    if !is_production_rust_file(path) {
        return;
    }
    let text = fs::read_to_string(path).expect("read rust source");
    let relative = path.strip_prefix(root).expect("relative path");
    let crate_root = is_crate_root_source(path);
    matches.extend(find_allows(&relative.to_string_lossy(), &text, crate_root));
}

fn is_production_rust_file(path: &Path) -> bool {
    if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
        return false;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    !(name == "tests.rs" || name.ends_with("_test.rs") || name.ends_with("_tests.rs"))
}

fn is_crate_root_source(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("lib.rs" | "main.rs")
    ) && path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        == Some("src")
}

fn find_allows(file: &str, text: &str, crate_root: bool) -> Vec<AllowMatch> {
    let lines = text.lines().collect::<Vec<_>>();
    let mut matches = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let trimmed = lines[index].trim_start();
        if trimmed.starts_with("//") || !trimmed.starts_with("#") {
            index += 1;
            continue;
        }
        let start = index;
        let mut snippet = trimmed.to_owned();
        while !snippet.contains(']') && index + 1 < lines.len() {
            index += 1;
            snippet.push(' ');
            snippet.push_str(lines[index].trim());
        }
        if let Some(form) = matched_form(&snippet) {
            if !(crate_root && is_crate_root_test_exemption(&snippet)) {
                matches.push(AllowMatch {
                    file: file.to_owned(),
                    line: start + 1,
                    form,
                    snippet,
                });
            }
        }
        index += 1;
    }
    matches
}

fn matched_form(snippet: &str) -> Option<&'static str> {
    let compact = snippet
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    if compact.starts_with("#[allow(") {
        Some("#[allow(")
    } else if compact.starts_with("#![allow(") {
        Some("#![allow(")
    } else if compact.starts_with("#[expect(") {
        Some("#[expect(")
    } else if compact.starts_with("#![expect(") {
        Some("#![expect(")
    } else if compact.starts_with("#[cfg_attr(")
        && (compact.contains(",allow(") || compact.contains(",expect("))
    {
        Some("#[cfg_attr(..., allow|expect(")
    } else if compact.starts_with("#![cfg_attr(")
        && (compact.contains(",allow(") || compact.contains(",expect("))
    {
        Some("#![cfg_attr(..., allow|expect(")
    } else {
        None
    }
}

fn is_crate_root_test_exemption(snippet: &str) -> bool {
    snippet
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>()
        .starts_with("#![cfg_attr(test,allow(")
}
