//! Workspace file listing for `@` mentions (gitignore-respected).

use std::path::Path;
use std::process::Command;

const MAX_WORKSPACE_FILES: usize = 2_000;

/// List workspace-relative file paths for the mention picker.
///
/// Prefer `git ls-files` (gitignore-respected). Fall back to a bounded
/// directory walk that skips common ignore directories when not in a git tree.
pub fn list_workspace_files(root: &Path) -> Vec<String> {
    if let Some(files) = git_ls_files(root) {
        return files;
    }
    walk_workspace_files(root)
}

fn git_ls_files(root: &Path) -> Option<Vec<String>> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut files = output
        .stdout
        .split(|&b| b == 0)
        .filter(|chunk| !chunk.is_empty())
        .filter_map(|chunk| String::from_utf8(chunk.to_vec()).ok())
        .filter(|path| !path.is_empty() && !path.ends_with('/'))
        .collect::<Vec<_>>();
    files.sort();
    files.truncate(MAX_WORKSPACE_FILES);
    Some(files)
}

fn walk_workspace_files(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || is_skipped_dir(&name) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                if let Ok(rel) = path.strip_prefix(root) {
                    files.push(rel.to_string_lossy().replace('\\', "/"));
                    if files.len() >= MAX_WORKSPACE_FILES {
                        files.sort();
                        return files;
                    }
                }
            }
        }
    }
    files.sort();
    files
}

fn is_skipped_dir(name: &str) -> bool {
    matches!(
        name,
        "target"
            | "node_modules"
            | ".git"
            | ".hg"
            | ".svn"
            | "dist"
            | "build"
            | "__pycache__"
            | ".venv"
            | "venv"
    )
}

/// Fuzzy-filter paths by subsequence match on the query (case-insensitive).
pub fn filter_workspace_files(files: &[String], query: &str) -> Vec<String> {
    if query.is_empty() {
        return files.iter().take(50).cloned().collect();
    }
    let needle = query.to_lowercase();
    let mut scored = files
        .iter()
        .filter_map(|path| {
            let hay = path.to_lowercase();
            score_match(&hay, &needle).map(|score| (score, path.clone()))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().take(50).map(|(_, path)| path).collect()
}

fn score_match(haystack: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    if let Some(idx) = haystack.find(needle) {
        // Prefer earlier and shorter paths.
        return Some(idx as i32 * 10 + haystack.len() as i32);
    }
    // Subsequence fuzzy match.
    let mut hchars = haystack.chars();
    for nc in needle.chars() {
        loop {
            let hc = hchars.next()?;
            if hc == nc {
                break;
            }
        }
    }
    Some(1_000 + haystack.len() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn filter_prefers_substring_hits() {
        let files = vec![
            "src/main.rs".to_owned(),
            "crates/euler-cli/src/ui/app.rs".to_owned(),
            "README.md".to_owned(),
        ];
        let hits = filter_workspace_files(&files, "app.rs");
        assert_eq!(hits[0], "crates/euler-cli/src/ui/app.rs");
    }

    #[test]
    fn walk_skips_target_and_dot_dirs() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("src")).expect("src");
        fs::create_dir_all(temp.path().join("target/debug")).expect("target");
        fs::create_dir_all(temp.path().join(".git")).expect("git");
        fs::write(temp.path().join("src/lib.rs"), "x").expect("write");
        fs::write(temp.path().join("target/debug/x"), "x").expect("write target");
        fs::write(temp.path().join(".git/config"), "x").expect("write git");
        let files = walk_workspace_files(temp.path());
        assert_eq!(files, vec!["src/lib.rs".to_owned()]);
    }
}
