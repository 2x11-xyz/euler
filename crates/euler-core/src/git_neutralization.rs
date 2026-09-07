//! Neutralization for Euler's own `git` invocations (ADR 0021 row G).
//!
//! `git_status` and `git_diff` read a repository the agent can write. Git
//! turns several pieces of repository-supplied configuration into executable
//! code: hooks, clean/process filter drivers, textconv and external diff
//! helpers, LFS smudge, a `core.fsmonitor` helper path, and — through a
//! recursive submodule spawn — any of the same configured in
//! `.git/modules/<name>/config`. A write to `.git/config` or `.gitattributes`
//! followed by `git status` must not become arbitrary execution.
//!
//! Every git invocation Euler itself makes goes through here, not only the
//! tools: the `@`-mention picker's `git ls-files` runs on a user keypress and
//! is just as reachable. Commands the agent runs itself through `run_shell`
//! are confined by the sandbox instead, and keep the repository's own
//! configuration.
//!
//! Residual risk: the driver probe and the real command are two processes, so
//! a writer that adds a `filter.*.clean` between them is not covered. Under
//! the Linux sandbox that driver would run inside the sandbox anyway; on a
//! host backend it needs an agent racing its own tool call. Closing it needs a
//! single-process git binding, not a third probe.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

/// Configuration Git must not take from the repository.
///
/// `core.hooksPath=/dev/null` disables every hook; `safe.bareRepository`
/// rejects an implicitly discovered bare repository (a `.git` directory the
/// agent planted elsewhere); `attr.tree=` and `core.attributesFile=` remove
/// the tree-based and global attribute sources that select filter drivers;
/// `diff.ignoreSubmodules=dirty` stops the recursive submodule spawn, which
/// would otherwise run a driver configured in the submodule's own config with
/// only the superproject's blanking applied.
const NEUTRALIZED_CONFIG: &[&str] = &[
    "core.hooksPath=/dev/null",
    "safe.bareRepository=explicit",
    "attr.tree=",
    "core.attributesFile=",
    "diff.ignoreSubmodules=dirty",
];

/// The configuration keys whose values Git executes. `core.fsmonitor` is here
/// too: it accepts a hook pathname as well as a boolean, and reading it in the
/// same pass is what keeps the probe to one launch.
const EXECUTABLE_CONFIG_PATTERN: &str = r"^(core\.fsmonitor|filter\..*\.(clean|process))$";

// The redirect list is shared verbatim with `build.rs`, which cannot depend
// on this crate. One list, two includes.
include!("git_redirect_env.rs");

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
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitNeutralization {
    config_args: Vec<String>,
    env: Vec<(OsString, OsString)>,
}

impl GitNeutralization {
    /// The invocation used for the probe itself.
    ///
    /// It carries no `core.fsmonitor` override, because the probe is what
    /// reads that key: an override in its own argv would make every repository
    /// report the overridden value, leaving the built-in daemon unreachable
    /// and every command paying for a full worktree scan.
    pub(crate) fn probe() -> Self {
        Self {
            config_args: config_args(None),
            env: base_env(),
        }
    }

    /// The strictest invocation, for a caller with no probe of its own.
    pub(crate) fn strict() -> Self {
        Self {
            config_args: config_args(Some(FsmonitorOverride::Disabled)),
            env: base_env(),
        }
    }

    /// The invocation used for a tool's real command, after the probe.
    pub(crate) fn new(fsmonitor: FsmonitorOverride, filter_drivers: &[String]) -> Self {
        Self {
            config_args: config_args(Some(fsmonitor)),
            env: base_env()
                .into_iter()
                .chain(filter_override_env(filter_drivers))
                .collect(),
        }
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

    /// The arguments that read every executable configuration key at once.
    pub(crate) fn probe_args(&self) -> Vec<&str> {
        self.args(&[
            "config",
            "--null",
            "--get-regexp",
            EXECUTABLE_CONFIG_PATTERN,
        ])
    }

    /// Drop the inherited redirect family from one of Euler's git commands.
    /// The sandbox clears the whole environment, so this is what protects the
    /// host backend and the invocations that never enter a sandbox at all.
    pub(crate) fn strip_inherited_redirects(&self, command: &mut Command) {
        for (name, _) in std::env::vars_os() {
            let redirects = name.to_str().is_some_and(is_redirecting_git_env_name);
            if redirects && !self.env.iter().any(|(kept, _)| *kept == name) {
                command.env_remove(name);
            }
        }
    }
}

fn config_args(fsmonitor: Option<FsmonitorOverride>) -> Vec<String> {
    NEUTRALIZED_CONFIG
        .iter()
        .map(|config| (*config).to_owned())
        .chain(fsmonitor.map(|fsmonitor| fsmonitor.config().to_owned()))
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

/// What one repository has configured that Git would execute.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ExecutableGitConfig {
    /// The effective `core.fsmonitor` value, if any.
    pub(crate) fsmonitor: Option<String>,
    /// The `filter.<name>` drivers with a `clean` or `process` command.
    pub(crate) filter_drivers: Vec<String>,
}

/// Parse one `git config --null --get-regexp` result.
///
/// Records are `key\nvalue\0`; a valueless key has no newline. Git lists
/// scopes in increasing precedence, so the last `core.fsmonitor` record is the
/// effective one.
pub(crate) fn parse_executable_config(output: &str) -> ExecutableGitConfig {
    let mut config = ExecutableGitConfig::default();
    for record in output.split('\0').filter(|record| !record.is_empty()) {
        let (key, value) = record.split_once('\n').unwrap_or((record, ""));
        if key == "core.fsmonitor" {
            config.fsmonitor = Some(value.to_owned());
            continue;
        }
        if let Some(driver) = key
            .strip_suffix(".clean")
            .or_else(|| key.strip_suffix(".process"))
        {
            if driver.starts_with("filter.") {
                config.filter_drivers.push(driver.to_owned());
            }
        }
    }
    config.filter_drivers.sort();
    config.filter_drivers.dedup();
    config
}

/// Decide the `core.fsmonitor` override from the effective configured value.
///
/// Only the boolean spellings Git accepts directly are preserved; anything
/// else — including a hook pathname — is disabled. The caller supplies the
/// separate evidence that Git actually has the built-in daemon.
pub(crate) fn fsmonitor_override(configured: Option<&str>, has_daemon: bool) -> FsmonitorOverride {
    let enabled = configured.is_some_and(|configured| {
        ["true", "yes", "on"]
            .iter()
            .any(|value| configured.eq_ignore_ascii_case(value))
    });
    if enabled && has_daemon {
        FsmonitorOverride::BuiltIn
    } else {
        FsmonitorOverride::Disabled
    }
}

/// Whether this Git build advertises the built-in filesystem monitor daemon.
///
/// The answer is a property of the binary, so it is asked once per process.
pub(crate) fn cached_fsmonitor_daemon_support(probe: impl FnOnce() -> Option<String>) -> bool {
    static SUPPORT: OnceLock<bool> = OnceLock::new();
    if let Some(support) = SUPPORT.get() {
        return *support;
    }
    // Only a completed probe is remembered. A probe that timed out says
    // nothing about this Git build, and caching its failure would make one
    // slow moment cost every later repository a full worktree scan.
    let Some(build_options) = probe() else {
        return false;
    };
    let support = build_options
        .lines()
        .any(|line| line.trim() == "feature: fsmonitor--daemon");
    let _ = SUPPORT.set(support);
    support
}

/// Build one of Euler's own git invocations that runs directly on the host.
///
/// This is the shared entry point for every Euler-owned git call outside the
/// tool path — the `@`-mention picker today. It uses the strictest
/// neutralization because such a caller has no probe of its own; a caller that
/// wants the built-in fsmonitor daemon probes and uses
/// [`GitNeutralization::new`].
pub fn host_git_command(root: &Path, args: &[&str]) -> Command {
    let neutralization = GitNeutralization::strict();
    let mut command = Command::new("git");
    command.arg("-C").arg(root);
    command.args(neutralization.args(args));
    for (name, value) in neutralization.env() {
        command.env(name, value);
    }
    neutralization.strip_inherited_redirects(&mut command);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_config_reads_fsmonitor_and_drivers_in_one_pass() {
        let config = parse_executable_config(
            "filter.evil.clean\ntouch marker\0core.fsmonitor\ntrue\0filter.evil.process\nx\0\
             filter.lfs.clean\ngit-lfs clean\0",
        );

        assert_eq!(config.fsmonitor.as_deref(), Some("true"));
        assert_eq!(
            config.filter_drivers,
            vec!["filter.evil".to_owned(), "filter.lfs".to_owned()]
        );
    }

    #[test]
    fn the_last_fsmonitor_record_is_the_effective_one() {
        let config = parse_executable_config("core.fsmonitor\ntrue\0core.fsmonitor\n./helper\0");

        assert_eq!(config.fsmonitor.as_deref(), Some("./helper"));
        assert_eq!(
            fsmonitor_override(config.fsmonitor.as_deref(), true),
            FsmonitorOverride::Disabled
        );
    }

    #[test]
    fn the_probe_carries_no_fsmonitor_override_of_its_own() {
        let probe = GitNeutralization::probe();
        let args = probe.probe_args();

        assert!(
            !args
                .iter()
                .any(|argument| argument.starts_with("core.fsmonitor")),
            "{args:?}"
        );
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-c", "core.hooksPath=/dev/null"]));
    }

    #[test]
    fn a_daemon_backed_true_survives_onto_the_real_command() {
        let config = parse_executable_config("core.fsmonitor\ntrue\0");
        let fsmonitor = fsmonitor_override(config.fsmonitor.as_deref(), true);
        let neutralization = GitNeutralization::new(fsmonitor, &[]);
        let args = neutralization.args(&["status"]);

        assert_eq!(fsmonitor, FsmonitorOverride::BuiltIn);
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-c", "core.fsmonitor=true"]));
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
        assert!(env.contains(&(
            "GIT_CONFIG_KEY_2".to_owned(),
            "filter.evil.required".to_owned()
        )));
        assert!(env.contains(&("GIT_CONFIG_VALUE_2".to_owned(), "false".to_owned())));
        assert!(env.contains(&("GIT_LFS_SKIP_SMUDGE".to_owned(), "1".to_owned())));
    }

    #[test]
    fn hooks_attributes_and_submodule_recursion_are_neutralized_everywhere() {
        let neutralization = GitNeutralization::strict();
        let args = neutralization.args(&["status"]);

        for config in [
            "core.hooksPath=/dev/null",
            "safe.bareRepository=explicit",
            "attr.tree=",
            "core.attributesFile=",
            "diff.ignoreSubmodules=dirty",
            "core.fsmonitor=false",
        ] {
            assert!(
                args.windows(2).any(|pair| pair == ["-c", config]),
                "{config} missing from {args:?}"
            );
        }
        assert_eq!(args.last(), Some(&"status"));
    }

    #[test]
    fn redirecting_git_variables_are_matched_exactly_or_by_indexed_family() {
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_11",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG_PARAMETERS",
        ] {
            assert!(is_redirecting_git_env_name(name), "{name}");
        }
        for name in [
            "GIT_TERMINAL_PROMPT",
            "GIT_OPTIONAL_LOCKS",
            // A bare `GIT_CONFIG` prefix match would sweep these up too.
            "GIT_CONFIGURATION",
            "GIT_CONFIG_PARAMETERS_LIKE",
        ] {
            assert!(!is_redirecting_git_env_name(name), "{name}");
        }
    }
}
