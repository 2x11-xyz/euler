// Environment that points Git at another repository, index, or configuration
// file. Euler resolves the repository from the workspace root, so every one of
// these is removed before its own git starts.
//
// This file is `include!`d by both `src/git_neutralization.rs` and `build.rs`.
// A build script cannot depend on the crate it builds, and two lists that
// agree today would drift the first time a name is added — which is the bug
// this file exists to prevent. It therefore has no imports and declares
// nothing but these two constants.

/// Names matched exactly.
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
    // Git's own mechanism for handing `-c` overrides to child processes, and
    // still honoured: an inherited value injects configuration into every
    // invocation with exactly the power this list exists to deny.
    "GIT_CONFIG_PARAMETERS",
];

/// The indexed half of the `GIT_CONFIG_COUNT` family, whose indices are
/// unbounded and so are matched by prefix.
const REDIRECTING_GIT_ENV_PREFIXES: &[&str] = &["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];

fn is_redirecting_git_env_name(name: &str) -> bool {
    REDIRECTING_GIT_ENV.contains(&name)
        || REDIRECTING_GIT_ENV_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}
