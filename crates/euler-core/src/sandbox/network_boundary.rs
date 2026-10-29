//! The host facts a `workspace` boundary (ADR 0026 section 3) reaches,
//! resolved once per session: the private `HOME`, the named toolchain
//! caches, the profile's `writable_paths`, the credential stores, and the
//! inherited environment under ADR 0021 row C.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use super::{SandboxUnavailableReason, WorkspaceNetworkAccess};

/// Default locations of credentials common CLIs read without a prompt,
/// relative to the real home. On macOS their reads are denied unless
/// `credentials: "pass"` passes them; Linux never mounts the real home.
const CREDENTIAL_STORES: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".config/gh",
    ".config/op",
    ".kube",
    ".docker/config.json",
    ".netrc",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials.toml",
    ".terraform.d",
];

/// Git identity and configuration, passed with the credential stores under
/// `credentials: "pass"` so commits and credential helpers behave as on the
/// host.
const PASSED_GIT_CONFIG: &[&str] = &[".gitconfig", ".config/git"];

/// macOS stores whose reads stay denied even under `credentials: "pass"`:
/// the keychains and browser profiles. The Euler home is added at runtime.
const ALWAYS_DENIED_STORES: &[&str] = &[
    "Library/Keychains",
    "Library/Cookies",
    "Library/Safari",
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/Chromium",
    "Library/Application Support/BraveSoftware",
    "Library/Application Support/Microsoft Edge",
    "Library/Application Support/Arc",
    "Library/Application Support/Firefox",
];

/// Euler's own provider credentials. They never reach a command, even under
/// `credentials: "pass"`.
const EULER_PROVIDER_KEYS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    "XAI_API_KEY",
];

/// ADR 0021 row C's hard denylist for sandboxed commands: loader and
/// injection variables with no legitimate user use, plus `GIT_SSH_COMMAND`,
/// which only Euler may set. Applied under every `credentials` value.
fn denied_by_row_c(name: &str) -> bool {
    matches!(
        name,
        "LD_PRELOAD"
            | "LD_AUDIT"
            | "LD_LIBRARY_PATH"
            | "DYLD_INSERT_LIBRARIES"
            | "BASH_ENV"
            | "ENV"
            | "GIT_CONFIG_COUNT"
            | "GIT_CONFIG_GLOBAL"
            | "GIT_SSH_COMMAND"
    ) || name.starts_with("GIT_CONFIG_KEY_")
        || name.starts_with("GIT_CONFIG_VALUE_")
}

/// Inherited variables that point into the real home, which the private
/// `HOME` replaces. Their defaults then follow `HOME`.
const REAL_HOME_XDG: &[&str] = &["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"];

/// Set for every sandboxed command so pagers and color never wait on a TTY.
const PAGER_ENVIRONMENT: &[(&str, &str)] = &[
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("TERM", "dumb"),
    ("NO_COLOR", "1"),
];

/// A credential store passed under `credentials: "pass"`.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct PassedCredential {
    /// The store's canonical host path.
    pub(super) real: PathBuf,
    /// The same relative path under the private `HOME`.
    pub(super) under_home: PathBuf,
    /// The real path is covered by `writable_paths`, so a CLI can refresh a
    /// token in place.
    pub(super) writable: bool,
}

#[derive(Debug)]
pub(super) struct NetworkBoundary {
    /// The private per-session `HOME`, canonical.
    pub(super) home: PathBuf,
    /// Keeps a fallback `HOME` for the session and removes it afterwards.
    pub(super) _home_guard: Option<tempfile::TempDir>,
    /// Named toolchain caches that exist on the host: the variable that
    /// points a toolchain at each (Cargo finds its caches through
    /// `CARGO_HOME`) and its canonical real path.
    pub(super) caches: Vec<(Option<&'static str>, PathBuf)>,
    pub(super) writable_paths: Vec<PathBuf>,
    pub(super) pass_credentials: bool,
    pub(super) passed_credentials: Vec<PassedCredential>,
    /// Paths whose reads Seatbelt denies.
    pub(super) denied_reads: Vec<PathBuf>,
    /// The host temporary directory, exported as `TMPDIR` on macOS.
    pub(super) host_tmpdir: PathBuf,
}

impl NetworkBoundary {
    /// Resolve the boundary from the host environment. `home` is
    /// `<session dir>/home`; without one, a fresh directory under
    /// `<euler home>/state/homes` lives as long as the sandbox.
    pub(super) fn resolve(
        access: &WorkspaceNetworkAccess,
        home: Option<PathBuf>,
    ) -> Result<Self, SandboxUnavailableReason> {
        fn cannot<E>(_: E) -> SandboxUnavailableReason {
            SandboxUnavailableReason::CannotEnforce
        }
        let (home, home_guard) = match home {
            Some(home) => {
                crate::home::ensure_private_dir(&home).map_err(cannot)?;
                (home, None)
            }
            None => {
                let homes = crate::EulerHome::resolve()
                    .map_err(cannot)?
                    .root()
                    .join("state")
                    .join("homes");
                crate::home::ensure_private_dir(&homes).map_err(cannot)?;
                let guard = tempfile::Builder::new()
                    .prefix("home-")
                    .tempdir_in(&homes)
                    .map_err(cannot)?;
                (guard.path().to_path_buf(), Some(guard))
            }
        };
        let home = home.canonicalize().map_err(cannot)?;
        let euler_home = crate::EulerHome::resolve()
            .ok()
            .and_then(|euler| euler.root().canonicalize().ok());
        let real_home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|real| real.canonicalize().ok());
        let host_tmpdir = std::env::temp_dir().canonicalize().map_err(cannot)?;
        Ok(Self::from_host(
            access,
            home,
            home_guard,
            HostView {
                real_home: real_home.as_deref(),
                euler_home: euler_home.as_deref(),
                host_tmpdir,
                variable: &|name| std::env::var_os(name),
            },
        ))
    }

    fn from_host(
        access: &WorkspaceNetworkAccess,
        home: PathBuf,
        home_guard: Option<tempfile::TempDir>,
        host: HostView<'_>,
    ) -> Self {
        let writable_paths = access.writable_paths.clone();
        let stores = |names: &'static [&'static str]| {
            host.real_home
                .into_iter()
                .flat_map(move |real_home| names.iter().map(move |name| (*name, real_home)))
        };
        let passed_credentials = if access.pass_credentials {
            stores(CREDENTIAL_STORES)
                .chain(stores(PASSED_GIT_CONFIG))
                .filter_map(|(name, real_home)| {
                    let real = real_home.join(name).canonicalize().ok()?;
                    let writable = writable_paths.iter().any(|path| real.starts_with(path));
                    Some(PassedCredential {
                        real,
                        under_home: home.join(name),
                        writable,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut denied_reads = Vec::new();
        if !access.pass_credentials {
            denied_reads.extend(stores(CREDENTIAL_STORES).map(|(name, real_home)| {
                let path = real_home.join(name);
                // Seatbelt matches the resolved path, so deny a symlinked
                // store where it really lives.
                path.canonicalize().unwrap_or(path)
            }));
        }
        denied_reads.extend(stores(ALWAYS_DENIED_STORES).map(|(name, real_home)| {
            let path = real_home.join(name);
            path.canonicalize().unwrap_or(path)
        }));
        denied_reads.extend(host.euler_home.map(Path::to_path_buf));
        Self {
            caches: named_caches(host.variable, host.real_home),
            home,
            _home_guard: home_guard,
            writable_paths,
            pass_credentials: access.pass_credentials,
            passed_credentials,
            denied_reads,
            host_tmpdir: host.host_tmpdir,
        }
    }

    /// The environment of a sandboxed command: the parent environment
    /// minus row C's denylist, Euler's parent-control variables, and
    /// secret-shaped variables (only Euler's own provider keys under
    /// `credentials: "pass"`); then the toolchain variables, the private
    /// `HOME`, `TMPDIR`, `PATH`, the cache variables, the pager variables,
    /// and the caller's `extra`. Later entries win.
    pub(super) fn environment(
        &self,
        parent: impl IntoIterator<Item = (OsString, OsString)>,
        toolchain: impl IntoIterator<Item = (OsString, OsString)>,
        tmpdir: &Path,
        path: OsString,
        extra: &[(OsString, OsString)],
    ) -> Vec<(OsString, OsString)> {
        let mut environment: Vec<(OsString, OsString)> = parent
            .into_iter()
            .filter(|(name, _)| self.inherits(name))
            .collect();
        environment.extend(toolchain);
        let set = |name: &str, value: &OsStr| (OsString::from(name), value.to_os_string());
        environment.push(set("HOME", self.home.as_os_str()));
        environment.push(set("XDG_CACHE_HOME", self.home.join(".cache").as_os_str()));
        environment.push(set("TMPDIR", tmpdir.as_os_str()));
        environment.push(set("PATH", &path));
        environment.extend(
            self.caches
                .iter()
                .filter_map(|(name, path)| name.map(|name| set(name, path.as_os_str()))),
        );
        environment.extend(
            PAGER_ENVIRONMENT
                .iter()
                .map(|(name, value)| set(name, OsStr::new(value))),
        );
        environment.extend(extra.iter().cloned());
        environment
    }

    fn inherits(&self, name: &OsStr) -> bool {
        if crate::tools::is_parent_control_env_name(name) {
            return false;
        }
        let secret = if self.pass_credentials {
            name.to_str().is_some_and(|name| {
                EULER_PROVIDER_KEYS.contains(&name.to_ascii_uppercase().as_str())
            })
        } else {
            crate::redaction::is_secret_env_name(name)
        };
        // A name that is not UTF-8 cannot be checked against the denylist,
        // so it does not pass.
        let Some(text) = name.to_str() else {
            return false;
        };
        !secret && !denied_by_row_c(text) && !REAL_HOME_XDG.contains(&text)
    }

    /// Every host path a command may write besides the workspace, `/tmp`,
    /// and `TMPDIR`: the caches, `writable_paths`, and the private `HOME`.
    pub(super) fn writable(&self) -> impl Iterator<Item = &Path> {
        self.caches
            .iter()
            .map(|(_, path)| path.as_path())
            .chain(self.writable_paths.iter().map(PathBuf::as_path))
    }

    /// Point each passed store's path under the private `HOME` at the real
    /// store, for Seatbelt, which cannot remap paths. An existing entry is
    /// left alone.
    pub(super) fn link_passed_credentials(&self) {
        for store in &self.passed_credentials {
            if store.under_home.symlink_metadata().is_ok() {
                continue;
            }
            if let Some(parent) = store.under_home.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::os::unix::fs::symlink(&store.real, &store.under_home);
        }
    }
}

/// What [`NetworkBoundary::from_host`] reads from the host.
struct HostView<'a> {
    real_home: Option<&'a Path>,
    euler_home: Option<&'a Path>,
    host_tmpdir: PathBuf,
    variable: &'a dyn Fn(&str) -> Option<OsString>,
}

/// The named toolchain caches (ADR 0026 resolved question 13) that exist
/// on the host, each with the variable that points a toolchain at it. A
/// variable the user set wins over the default location. A cache root
/// (`~/.cache`) is never included, so credential files beside these caches
/// stay out; a missing cache is created in the private `HOME` instead.
fn named_caches(
    variable: &dyn Fn(&str) -> Option<OsString>,
    home: Option<&Path>,
) -> Vec<(Option<&'static str>, PathBuf)> {
    let from = |name: &str| variable(name).map(PathBuf::from);
    let under_home = |relative: &str| home.map(|home| home.join(relative));
    let xdg_cache = from("XDG_CACHE_HOME").or_else(|| under_home(".cache"));
    let in_xdg = |relative: &str| xdg_cache.as_ref().map(|cache| cache.join(relative));
    let pip_default = if cfg!(target_os = "macos") {
        under_home("Library/Caches/pip")
    } else {
        in_xdg("pip")
    };
    let cargo_home = from("CARGO_HOME").or_else(|| under_home(".cargo"));
    let gopath = variable("GOPATH")
        .and_then(|value| std::env::split_paths(&value).next())
        .or_else(|| under_home("go"));
    let candidates: [(Option<&'static str>, Option<PathBuf>); 7] = [
        (Some("PIP_CACHE_DIR"), from("PIP_CACHE_DIR").or(pip_default)),
        (
            Some("UV_CACHE_DIR"),
            from("UV_CACHE_DIR").or_else(|| in_xdg("uv")),
        ),
        (
            Some("npm_config_cache"),
            from("npm_config_cache")
                .or_else(|| from("NPM_CONFIG_CACHE"))
                .or_else(|| under_home(".npm")),
        ),
        (
            None,
            cargo_home.as_ref().map(|cargo| cargo.join("registry")),
        ),
        (None, cargo_home.as_ref().map(|cargo| cargo.join("git"))),
        (
            Some("GOMODCACHE"),
            from("GOMODCACHE").or_else(|| gopath.map(|gopath| gopath.join("pkg/mod"))),
        ),
        (
            Some("HF_HUB_CACHE"),
            from("HF_HUB_CACHE")
                .or_else(|| from("HF_HOME").map(|hf| hf.join("hub")))
                .or_else(|| in_xdg("huggingface/hub")),
        ),
    ];
    candidates
        .into_iter()
        .filter_map(|(name, path)| {
            let path = path?.canonicalize().ok()?;
            let is_a_real_subtree = path.is_dir()
                && path.components().count() > 2
                && home.is_none_or(|home| !home.starts_with(&path));
            is_a_real_subtree.then_some((name, path))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn host<'a>(
        real_home: &'a Path,
        variable: &'a dyn Fn(&str) -> Option<OsString>,
    ) -> HostView<'a> {
        HostView {
            real_home: Some(real_home),
            euler_home: None,
            host_tmpdir: PathBuf::from("/tmp"),
            variable,
        }
    }

    fn names(environment: &[(OsString, OsString)]) -> BTreeMap<String, String> {
        environment
            .iter()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn named_caches_bind_only_existing_cache_subdirectories() {
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().canonicalize().expect("canonical");
        for cache in [
            ".cache/uv",
            ".npm",
            ".cargo/registry",
            ".cache/huggingface/hub",
        ] {
            fs::create_dir_all(home.join(cache)).expect("cache");
        }
        fs::write(home.join(".cache/huggingface/token"), "secret").expect("token");
        let none = |_: &str| None;
        let caches = named_caches(&none, Some(&home));
        let paths: Vec<_> = caches.iter().map(|(_, path)| path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                home.join(".cache/uv"),
                home.join(".npm"),
                home.join(".cargo/registry"),
                home.join(".cache/huggingface/hub"),
            ]
        );
        assert!(!paths.contains(&home.join(".cache")));
        assert!(!paths.contains(&home.join(".cache/huggingface")));
        assert_eq!(caches[0].0, Some("UV_CACHE_DIR"));
        assert_eq!(caches[2].0, None);

        // A variable the user set wins, but never names the home itself.
        let elsewhere = temp.path().join("elsewhere");
        fs::create_dir(&elsewhere).expect("elsewhere");
        let elsewhere = elsewhere.canonicalize().expect("canonical");
        let explicit = {
            let elsewhere = elsewhere.clone();
            let home = home.clone();
            move |name: &str| match name {
                "UV_CACHE_DIR" => Some(elsewhere.clone().into_os_string()),
                "PIP_CACHE_DIR" => Some(home.clone().into_os_string()),
                _ => None,
            }
        };
        let caches = named_caches(&explicit, Some(&home));
        assert!(caches.contains(&(Some("UV_CACHE_DIR"), elsewhere)));
        assert!(!caches.iter().any(|(_, path)| path == &home));
    }

    #[test]
    fn the_environment_follows_row_c_and_keeps_secrets_private() {
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().canonicalize().expect("canonical");
        let none = |_: &str| None;
        let boundary = NetworkBoundary::from_host(
            &WorkspaceNetworkAccess::default(),
            home.join("private"),
            None,
            host(&home, &none),
        );
        let parent = [
            ("HTTPS_PROXY", "http://proxy.example.com:3128"),
            ("LANG", "en_US.UTF-8"),
            ("LD_PRELOAD", "/evil.so"),
            ("DYLD_INSERT_LIBRARIES", "/evil.dylib"),
            ("BASH_ENV", "/evil.sh"),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "core.pager"),
            ("GIT_SSH_COMMAND", "ssh -o ProxyCommand=evil"),
            ("GITHUB_TOKEN", "ghp_example"),
            ("AWS_SECRET_ACCESS_KEY", "example"),
            ("ANTHROPIC_API_KEY", "sk-ant-example"),
            ("EULER_HOME", "/home/user/.euler"),
            ("XDG_CONFIG_HOME", "/home/user/.config"),
            ("HOME", "/home/user"),
            ("PAGER", "less"),
        ]
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));
        let environment = names(&boundary.environment(
            parent.clone(),
            [(OsString::from("CARGO_HOME"), OsString::from("/c"))],
            Path::new("/tmp"),
            OsString::from("/usr/bin:/bin"),
            &[(OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0"))],
        ));
        assert_eq!(environment["HTTPS_PROXY"], "http://proxy.example.com:3128");
        assert_eq!(environment["LANG"], "en_US.UTF-8");
        assert_eq!(environment["HOME"], home.join("private").to_string_lossy());
        assert_eq!(
            environment["XDG_CACHE_HOME"],
            home.join("private/.cache").to_string_lossy()
        );
        assert_eq!(environment["TMPDIR"], "/tmp");
        assert_eq!(environment["PATH"], "/usr/bin:/bin");
        assert_eq!(environment["CARGO_HOME"], "/c");
        assert_eq!(environment["PAGER"], "cat");
        assert_eq!(environment["GIT_PAGER"], "cat");
        assert_eq!(environment["TERM"], "dumb");
        assert_eq!(environment["NO_COLOR"], "1");
        assert_eq!(environment["GIT_TERMINAL_PROMPT"], "0");
        for denied in [
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "BASH_ENV",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_SSH_COMMAND",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "ANTHROPIC_API_KEY",
            "EULER_HOME",
            "XDG_CONFIG_HOME",
        ] {
            assert!(!environment.contains_key(denied), "{denied} passed");
        }

        // `credentials: "pass"` passes secret-shaped variables, except
        // Euler's own provider keys; row C's denylist still applies.
        let passing = NetworkBoundary::from_host(
            &WorkspaceNetworkAccess {
                writable_paths: Vec::new(),
                pass_credentials: true,
            },
            home.join("private"),
            None,
            host(&home, &none),
        );
        let environment =
            names(&passing.environment(parent, [], Path::new("/tmp"), OsString::from("/bin"), &[]));
        assert_eq!(environment["GITHUB_TOKEN"], "ghp_example");
        assert_eq!(environment["AWS_SECRET_ACCESS_KEY"], "example");
        for denied in [
            "ANTHROPIC_API_KEY",
            "EULER_HOME",
            "LD_PRELOAD",
            "GIT_SSH_COMMAND",
        ] {
            assert!(!environment.contains_key(denied), "{denied} passed");
        }
    }

    #[test]
    fn credential_stores_are_denied_unless_passed() {
        let temp = tempfile::tempdir().expect("temp");
        fs::create_dir(temp.path().join("real")).expect("real home");
        let real_home = temp.path().join("real").canonicalize().expect("canonical");
        fs::create_dir_all(real_home.join(".ssh")).expect("ssh");
        fs::create_dir_all(real_home.join(".aws")).expect("aws");
        fs::write(real_home.join(".gitconfig"), "[user]\n").expect("gitconfig");
        let private = temp.path().join("private");
        let none = |_: &str| None;

        let denying = NetworkBoundary::from_host(
            &WorkspaceNetworkAccess::default(),
            private.clone(),
            None,
            host(&real_home, &none),
        );
        assert!(denying.passed_credentials.is_empty());
        for store in [".ssh", ".aws", ".netrc", ".config/gh", "Library/Keychains"] {
            assert!(
                denying.denied_reads.contains(&real_home.join(store)),
                "{store} readable"
            );
        }

        let passing = NetworkBoundary::from_host(
            &WorkspaceNetworkAccess {
                writable_paths: vec![real_home.join(".aws")],
                pass_credentials: true,
            },
            private.clone(),
            None,
            host(&real_home, &none),
        );
        assert!(!passing.denied_reads.contains(&real_home.join(".ssh")));
        assert!(passing
            .denied_reads
            .contains(&real_home.join("Library/Keychains")));
        assert_eq!(
            passing.passed_credentials,
            vec![
                PassedCredential {
                    real: real_home.join(".ssh"),
                    under_home: private.join(".ssh"),
                    writable: false,
                },
                PassedCredential {
                    real: real_home.join(".aws"),
                    under_home: private.join(".aws"),
                    writable: true,
                },
                PassedCredential {
                    real: real_home.join(".gitconfig"),
                    under_home: private.join(".gitconfig"),
                    writable: false,
                },
            ]
        );
    }
}
