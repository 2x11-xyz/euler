//! Neutralization for Euler's own `git` invocations (ADR 0021 row G).
//!
//! `git_status` and `git_diff` read a repository the agent can write. Git
//! turns several pieces of repository-supplied configuration into executable
//! code: hooks, clean/process filter drivers, textconv and external diff
//! helpers, LFS smudge, and a `core.fsmonitor` helper path. A write to
//! `.git/config` or `.gitattributes` followed by `git status` must not become
//! arbitrary execution.
//!
//! Everything here applies to invocations Euler makes. Commands the agent runs
//! itself through `run_shell` are confined by the sandbox instead.

use std::ffi::OsString;

/// Configuration Git must not take from the repository.
///
/// `core.hooksPath=/dev/null` disables every hook; `safe.bareRepository`
/// rejects an implicitly discovered bare repository (a `.git` directory the
/// agent planted elsewhere); `attr.tree=` and `core.attributesFile=` remove
/// the tree-based and global attribute sources that select filter drivers.
const NEUTRALIZED_CONFIG: &[&str] = &[
    "core.hooksPath=/dev/null",
    "safe.bareRepository=explicit",
    "attr.tree=",
    "core.attributesFile=",
];

/// The Git configuration key pattern whose values Git executes.
const EXECUTABLE_FILTER_PATTERN: &str = r"^filter\..*\.(clean|process)$";

/// Environment that redirects Git at another repository, another index, or
/// another configuration file. Euler resolves the repository from the
/// workspace root, so every one of these is removed before Git starts.
pub(crate) const REDIRECTING_GIT_ENV_PREFIXES: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_CONFIG",
];

pub(crate) fn is_redirecting_git_env_name(name: &str) -> bool {
    REDIRECTING_GIT_ENV_PREFIXES
        .iter()
        .any(|prefix| name == *prefix || name.starts_with("GIT_CONFIG"))
}

/// Whether Git's built-in filesystem monitor daemon may stay enabled.
///
/// `core.fsmonitor` accepts a hook pathname as well as a boolean, so a
/// repository-local value is an execution channel. Blanket-disabling it costs
/// a full worktree scan on large repositories, so the effective value is
/// probed and preserved only for the built-in daemon.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FsmonitorOverride {
    Disabled,
    BuiltIn,
}

impl FsmonitorOverride {
    pub(crate) const fn config(self) -> &'static str {
        match self {
            Self::Disabled => "core.fsmonitor=false",
            Self::BuiltIn => "core.fsmonitor=true",
        }
    }
}

/// One neutralized Git invocation: the `-c` overrides that precede the
/// subcommand, and the environment the child receives.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct GitNeutralization {
    config_args: Vec<String>,
    env: Vec<(OsString, OsString)>,
}

impl GitNeutralization {
    /// The invocation used for the probes themselves: everything except the
    /// filter overrides, which the probes are what discovers.
    pub(crate) fn probe(fsmonitor: FsmonitorOverride) -> Self {
        Self {
            config_args: config_args(fsmonitor),
            env: base_env(),
        }
    }

    /// The invocation used for the tool's real command.
    pub(crate) fn new(fsmonitor: FsmonitorOverride, filter_drivers: &[String]) -> Self {
        let mut neutralization = Self::probe(fsmonitor);
        neutralization
            .env
            .extend(filter_override_env(filter_drivers));
        neutralization
    }

    /// The complete argument vector: overrides, then the caller's arguments.
    pub(crate) fn args<'a>(&'a self, command: &'a [&'a str]) -> Vec<&'a str> {
        self.config_args
            .iter()
            .flat_map(|config| ["-c", config.as_str()])
            .chain(command.iter().copied())
            .collect()
    }

    pub(crate) fn env(&self) -> &[(OsString, OsString)] {
        &self.env
    }

    /// The arguments that enumerate repository-configured filter drivers.
    pub(crate) fn filter_probe_args(&self) -> Vec<&str> {
        self.args(&[
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            EXECUTABLE_FILTER_PATTERN,
        ])
    }
}

fn config_args(fsmonitor: FsmonitorOverride) -> Vec<String> {
    NEUTRALIZED_CONFIG
        .iter()
        .map(|config| (*config).to_owned())
        .chain(std::iter::once(fsmonitor.config().to_owned()))
        .collect()
}

fn base_env() -> Vec<(OsString, OsString)> {
    [
        ("GIT_LFS_SKIP_SMUDGE", "1"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_OPTIONAL_LOCKS", "0"),
    ]
    .into_iter()
    .map(|(name, value)| (OsString::from(name), OsString::from(value)))
    .collect()
}

/// Blank every configured clean/process driver through the `GIT_CONFIG_KEY_n`
/// family, which outranks repository configuration. `required=false` keeps a
/// blanked driver from failing the command outright.
fn filter_override_env(drivers: &[String]) -> Vec<(OsString, OsString)> {
    let overrides = drivers
        .iter()
        .flat_map(|driver| {
            [
                (format!("{driver}.clean"), String::new()),
                (format!("{driver}.process"), String::new()),
                (format!("{driver}.required"), "false".to_owned()),
            ]
        })
        .collect::<Vec<_>>();
    if overrides.is_empty() {
        return Vec::new();
    }
    let mut env = vec![(
        OsString::from("GIT_CONFIG_COUNT"),
        OsString::from(overrides.len().to_string()),
    )];
    for (index, (key, value)) in overrides.into_iter().enumerate() {
        env.push((
            OsString::from(format!("GIT_CONFIG_KEY_{index}")),
            key.into(),
        ));
        env.push((
            OsString::from(format!("GIT_CONFIG_VALUE_{index}")),
            value.into(),
        ));
    }
    env
}

/// Read the filter driver names out of `git config --name-only --get-regexp`
/// output. Git separates records with NUL under `--null`.
pub(crate) fn filter_drivers(config_output: &str) -> Vec<String> {
    let mut drivers = config_output
        .split('\0')
        .filter_map(|key| {
            key.strip_suffix(".clean")
                .or_else(|| key.strip_suffix(".process"))
        })
        .filter(|driver| driver.starts_with("filter."))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    drivers.sort();
    drivers.dedup();
    drivers
}

/// Decide the `core.fsmonitor` override from the effective configured value.
///
/// Only the boolean spellings Git accepts directly are preserved; anything
/// else — including a hook pathname — is disabled. The caller supplies the
/// separate evidence that Git actually has the built-in daemon.
pub(crate) fn fsmonitor_override(configured: Option<&str>, has_daemon: bool) -> FsmonitorOverride {
    let Some(configured) = configured.and_then(|value| value.strip_suffix('\0')) else {
        return FsmonitorOverride::Disabled;
    };
    let enabled = ["true", "yes", "on"]
        .iter()
        .any(|value| configured.eq_ignore_ascii_case(value));
    if enabled && has_daemon {
        FsmonitorOverride::BuiltIn
    } else {
        FsmonitorOverride::Disabled
    }
}

/// Whether this Git build advertises the built-in filesystem monitor daemon.
pub(crate) fn advertises_fsmonitor_daemon(build_options: &str) -> bool {
    build_options
        .lines()
        .any(|line| line.trim() == "feature: fsmonitor--daemon")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_drivers_are_deduplicated_across_clean_and_process() {
        assert_eq!(
            filter_drivers("filter.evil.clean\0filter.evil.process\0filter.lfs.clean\0"),
            vec!["filter.evil".to_owned(), "filter.lfs".to_owned()]
        );
    }

    #[test]
    fn blanking_a_driver_outranks_repository_configuration() {
        let neutralization =
            GitNeutralization::new(FsmonitorOverride::Disabled, &["filter.evil".to_owned()]);
        let env = neutralization
            .env()
            .iter()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect::<Vec<_>>();

        assert!(env.contains(&("GIT_CONFIG_COUNT".to_owned(), "3".to_owned())));
        assert!(env.contains(&(
            "GIT_CONFIG_KEY_0".to_owned(),
            "filter.evil.clean".to_owned()
        )));
        assert!(env.contains(&("GIT_CONFIG_VALUE_0".to_owned(), String::new())));
        assert!(env.contains(&("GIT_LFS_SKIP_SMUDGE".to_owned(), "1".to_owned())));
    }

    #[test]
    fn hooks_and_attribute_sources_are_neutralized_on_every_invocation() {
        let neutralization = GitNeutralization::probe(FsmonitorOverride::Disabled);
        let args = neutralization.args(&["status"]);

        assert!(args
            .windows(2)
            .any(|pair| pair == ["-c", "core.hooksPath=/dev/null"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-c", "safe.bareRepository=explicit"]));
        assert!(args.windows(2).any(|pair| pair == ["-c", "attr.tree="]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-c", "core.attributesFile="]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-c", "core.fsmonitor=false"]));
        assert_eq!(args.last(), Some(&"status"));
    }

    #[test]
    fn only_the_builtin_daemon_survives_the_fsmonitor_probe() {
        assert_eq!(
            fsmonitor_override(Some("true\0"), true),
            FsmonitorOverride::BuiltIn
        );
        assert_eq!(
            fsmonitor_override(Some("true\0"), false),
            FsmonitorOverride::Disabled
        );
        assert_eq!(
            fsmonitor_override(Some(".git/hooks/evil\0"), true),
            FsmonitorOverride::Disabled
        );
        assert_eq!(fsmonitor_override(None, true), FsmonitorOverride::Disabled);
    }

    #[test]
    fn redirecting_git_variables_are_recognized() {
        assert!(is_redirecting_git_env_name("GIT_DIR"));
        assert!(is_redirecting_git_env_name("GIT_INDEX_FILE"));
        assert!(is_redirecting_git_env_name("GIT_CONFIG_COUNT"));
        assert!(is_redirecting_git_env_name("GIT_CONFIG_KEY_0"));
        assert!(is_redirecting_git_env_name(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES"
        ));
        assert!(!is_redirecting_git_env_name("GIT_TERMINAL_PROMPT"));
    }
}
