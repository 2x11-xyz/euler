//! Workspace file listing for `@` mentions (gitignore-respected).

use euler_core::host_git_command;
use std::path::Path;

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

/// The picker runs on a user keypress in a repository the agent can write, so
/// it is as reachable as the tools are: it goes through the same neutralization
/// (ADR 0021 row G), not a bare `git`.
fn git_ls_files(root: &Path) -> Option<Vec<String>> {
    let output = host_git_command(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )
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

    /// ADR 0021 row G: the picker runs `git ls-files` on a user keypress in a
    /// repository the agent can write. A repository-selected `core.fsmonitor`
    /// helper must not execute — `core.hooksPath=/dev/null` does not stop that
    /// one, only the fsmonitor override does.
    #[test]
    fn the_picker_does_not_run_a_repository_selected_fsmonitor_helper() {
        let Some(temp) = git_repository_fixture() else {
            return;
        };
        let root = temp.path();
        let helper = root.join("fsmonitor-helper");
        fs::write(&helper, "#!/bin/sh\ntouch fsmonitor-ran\nexit 1\n").expect("helper");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).expect("helper mode");
        }
        run_git(root, &["config", "core.fsmonitor", "./fsmonitor-helper"]);

        let files = list_workspace_files(root);

        assert!(
            !root.join("fsmonitor-ran").exists(),
            "picker ran a repository-selected fsmonitor helper: {files:?}"
        );
        assert!(files.contains(&"tracked.txt".to_owned()), "{files:?}");
    }

    fn git_repository_fixture() -> Option<tempfile::TempDir> {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return None;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        run_git(root, &["init", "--quiet"]);
        run_git(root, &["config", "user.email", "euler@example.invalid"]);
        run_git(root, &["config", "user.name", "Euler"]);
        fs::write(root.join("tracked.txt"), "original\n").expect("tracked file");
        run_git(root, &["add", "tracked.txt"]);
        run_git(root, &["commit", "--quiet", "-m", "seed"]);
        Some(temp)
    }

    fn run_git(root: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git fixture command");
        assert!(status.success(), "git {args:?}");
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
