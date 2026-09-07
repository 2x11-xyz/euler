use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let workspace = manifest_dir
        .parent()
        .and_then(Path::parent)
        .unwrap_or(&manifest_dir);

    let revision = git_output(workspace, &["rev-parse", "HEAD"]);
    let dirty = git_output(
        workspace,
        &["status", "--porcelain=v1", "--untracked-files=no"],
    )
    .map(|status| (!status.is_empty()).to_string());
    let (revision, dirty) = match (revision, dirty) {
        (Some(revision), Some(dirty)) => (revision, dirty),
        _ => ("unknown".to_owned(), "unknown".to_owned()),
    };

    let mut features = env::vars_os()
        .filter_map(|(key, _)| {
            key.to_str()?
                .strip_prefix("CARGO_FEATURE_")
                .map(str::to_owned)
        })
        .map(|feature| feature.to_ascii_lowercase().replace('_', "-"))
        .collect::<Vec<_>>();
    features.sort();

    println!("cargo:rustc-env=EULER_BUILD_GIT_SHA={revision}");
    println!("cargo:rustc-env=EULER_BUILD_GIT_DIRTY={dirty}");
    println!(
        "cargo:rustc-env=EULER_BUILD_FEATURES={}",
        features.join(",")
    );

    // Freeze source identity into the binary. Git is never consulted by a
    // running Euler process.
    if let Some(git_dir) =
        git_output(workspace, &["rev-parse", "--absolute-git-dir"]).map(PathBuf::from)
    {
        println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
        println!("cargo:rerun-if-changed={}", git_dir.join("index").display());
        if let (Some(common_dir), Some(reference)) = (
            git_output(workspace, &["rev-parse", "--git-common-dir"]),
            git_output(workspace, &["symbolic-ref", "-q", "HEAD"]),
        ) {
            let common_dir = PathBuf::from(common_dir);
            let common_dir = if common_dir.is_absolute() {
                common_dir
            } else {
                workspace.join(common_dir)
            };
            println!(
                "cargo:rerun-if-changed={}",
                common_dir.join(reference).display()
            );
            println!(
                "cargo:rerun-if-changed={}",
                common_dir.join("packed-refs").display()
            );
        }
    }
    // `git_dirty` means tracked source/index state differs from HEAD. Watch
    // every tracked path so an unstaged edit invalidates the compile-time
    // value; untracked files are deliberately outside this claim.
    if let Some(paths) = git_output(workspace, &["ls-files"]) {
        for path in paths.lines().filter(|path| !path.is_empty()) {
            println!("cargo:rerun-if-changed={}", workspace.join(path).display());
        }
    }
}

/// The same neutralization `src/git_neutralization.rs` applies to Euler's own
/// git, repeated here because a build script cannot depend on the crate it
/// builds. `git status` below runs hooks and a `core.fsmonitor` helper
/// otherwise (ADR 0021 row G).
///
/// It stops at the static overrides: there is no filter-driver probe here,
/// because building this crate already runs the repository's own build
/// scripts. The boundary a build script sits behind is the decision to build
/// the checkout at all, not the agent's tool call.
const NEUTRALIZED_CONFIG: &[&str] = &[
    "core.hooksPath=/dev/null",
    "safe.bareRepository=explicit",
    "attr.tree=",
    "core.attributesFile=",
    "diff.ignoreSubmodules=dirty",
    "core.fsmonitor=false",
];

/// Environment that points Git at another repository, index, or config file.
/// A developer with `GIT_DIR` set would otherwise stamp this build with
/// another checkout's revision.
const REDIRECTING_GIT_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_CONFIG_COUNT",
];

fn git_output(workspace: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(workspace);
    for name in REDIRECTING_GIT_ENV {
        command.env_remove(name);
    }
    for (name, _) in env::vars_os() {
        if name.to_str().is_some_and(|name| {
            name.starts_with("GIT_CONFIG_KEY_") || name.starts_with("GIT_CONFIG_VALUE_")
        }) {
            command.env_remove(name);
        }
    }
    for config in NEUTRALIZED_CONFIG {
        command.arg("-c").arg(config);
    }
    let output = command
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_owned())
}
