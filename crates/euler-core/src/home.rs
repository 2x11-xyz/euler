use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

const DEFAULT_HOME_CHILD: &str = ".euler";
const AUTH_FILE: &str = "auth.json";
const PREFERENCES_FILE: &str = "preferences.json";
const SESSIONS_DIR: &str = "sessions";
const EXTENSIONS_DIR: &str = "extensions";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EulerHome {
    root: PathBuf,
}

impl EulerHome {
    pub fn resolve() -> Result<Self, EulerHomeError> {
        Self::resolve_from_env(env::var_os("HOME"), env::var_os("EULER_HOME"))
    }

    pub fn resolve_from_env(
        home: Option<OsString>,
        euler_home: Option<OsString>,
    ) -> Result<Self, EulerHomeError> {
        let root = match euler_home {
            Some(path) => explicit_home(path)?,
            None => default_home(home)?,
        };
        ensure_private_dir(&root)?;
        Ok(Self {
            root: canonicalize_home(&root)?,
        })
    }

    pub fn from_root(root: impl Into<PathBuf>) -> Result<Self, EulerHomeError> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(EulerHomeError::RelativeOverride { path: root });
        }
        ensure_private_dir(&root)?;
        Ok(Self {
            root: canonicalize_home(&root)?,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn auth_path(&self) -> PathBuf {
        self.root.join(AUTH_FILE)
    }

    pub fn preferences_path(&self) -> PathBuf {
        self.root.join(PREFERENCES_FILE)
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join(SESSIONS_DIR)
    }

    pub fn extensions_dir(&self) -> PathBuf {
        self.root.join(EXTENSIONS_DIR)
    }

    pub fn ensure(&self) -> Result<(), EulerHomeError> {
        ensure_private_dir(&self.root)?;
        ensure_private_dir(&self.sessions_dir())
    }
}

#[derive(Debug, Error)]
pub enum EulerHomeError {
    #[error("HOME is unset; cannot resolve default Euler home")]
    MissingHome,
    #[error("HOME must be absolute to resolve default Euler home: {}", path.display())]
    RelativeHome { path: PathBuf },
    #[error("EULER_HOME must be an absolute path: {}", path.display())]
    RelativeOverride { path: PathBuf },
    #[error("Euler home path is not a directory: {}", path.display())]
    NotDirectory { path: PathBuf },
    #[error("Euler home path has a symlink loop: {}", path.display())]
    SymlinkLoop { path: PathBuf },
    #[error("Euler home path cannot be canonicalized: {}: {source}", path.display())]
    NonCanonicalizable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Euler home path is not writable: {}", path.display())]
    Unwritable { path: PathBuf },
    #[error("failed to access Euler home at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub(crate) fn ensure_private_dir(path: &Path) -> Result<(), EulerHomeError> {
    match fs::metadata(path) {
        Ok(metadata) => validate_dir(path, &metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|source| map_home_io(path, source))?;
            let metadata = fs::metadata(path).map_err(|source| map_home_io(path, source))?;
            validate_dir(path, &metadata)?;
        }
        Err(source) => return Err(map_home_io(path, source)),
    }
    set_dir_mode_0700(path).map_err(|source| map_home_io(path, source))?;
    Ok(())
}

fn explicit_home(path: OsString) -> Result<PathBuf, EulerHomeError> {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(EulerHomeError::RelativeOverride { path });
    }
    Ok(path)
}

fn default_home(home: Option<OsString>) -> Result<PathBuf, EulerHomeError> {
    let home = PathBuf::from(home.ok_or(EulerHomeError::MissingHome)?);
    if !home.is_absolute() {
        return Err(EulerHomeError::RelativeHome { path: home });
    }
    Ok(home.join(DEFAULT_HOME_CHILD))
}

fn canonicalize_home(path: &Path) -> Result<PathBuf, EulerHomeError> {
    fs::canonicalize(path).map_err(|source| map_canonicalize_error(path, source))
}

fn validate_dir(path: &Path, metadata: &fs::Metadata) -> Result<(), EulerHomeError> {
    if !metadata.is_dir() {
        return Err(EulerHomeError::NotDirectory {
            path: path.to_path_buf(),
        });
    }
    if !dir_is_writable(metadata) {
        return Err(EulerHomeError::Unwritable {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn dir_is_writable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o222 != 0
}

#[cfg(not(unix))]
fn dir_is_writable(metadata: &fs::Metadata) -> bool {
    !metadata.permissions().readonly()
}

fn set_dir_mode_0700(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

pub(crate) fn private_open_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

pub(crate) fn set_file_mode_0600(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Ok(())
    }
}

pub(crate) fn containing_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(unix)]
pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn map_home_io(path: &Path, source: io::Error) -> EulerHomeError {
    if is_symlink_loop(&source) {
        EulerHomeError::SymlinkLoop {
            path: path.to_path_buf(),
        }
    } else if source.kind() == io::ErrorKind::PermissionDenied {
        EulerHomeError::Unwritable {
            path: path.to_path_buf(),
        }
    } else {
        EulerHomeError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

fn map_canonicalize_error(path: &Path, source: io::Error) -> EulerHomeError {
    if is_symlink_loop(&source) {
        EulerHomeError::SymlinkLoop {
            path: path.to_path_buf(),
        }
    } else {
        EulerHomeError::NonCanonicalizable {
            path: path.to_path_buf(),
            source,
        }
    }
}

#[cfg(unix)]
fn is_symlink_loop(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ELOOP)
}

#[cfg(not(unix))]
fn is_symlink_loop(_error: &io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn resolves_default_home_from_home_env() {
        let temp = tempfile::tempdir().expect("temp dir");

        let home = EulerHome::resolve_from_env(Some(temp.path().into()), None).expect("home");

        assert_eq!(
            home.root(),
            fs::canonicalize(temp.path().join(".euler")).expect("canonical home")
        );
        assert!(home.root().is_dir());
        assert_eq!(home.auth_path(), home.root().join("auth.json"));
        assert_eq!(
            home.preferences_path(),
            home.root().join("preferences.json")
        );
        assert_eq!(home.extensions_dir(), home.root().join("extensions"));
    }

    #[test]
    fn resolves_absolute_euler_home_override() {
        let temp = tempfile::tempdir().expect("temp dir");
        let override_home = temp.path().join("custom-euler-home");

        let home = EulerHome::resolve_from_env(
            Some(temp.path().into()),
            Some(override_home.clone().into_os_string()),
        )
        .expect("home");

        assert_eq!(
            home.root(),
            fs::canonicalize(override_home).expect("canonical override")
        );
    }

    #[test]
    fn rejects_relative_euler_home_override() {
        let error = EulerHome::resolve_from_env(
            Some(OsString::from("/tmp")),
            Some(OsString::from("relative-home")),
        )
        .expect_err("relative override");

        assert!(matches!(error, EulerHomeError::RelativeOverride { .. }));
    }

    #[test]
    fn rejects_euler_home_pointing_at_file() {
        let temp = tempfile::tempdir().expect("temp dir");
        let file = temp.path().join("not-a-dir");
        fs::write(&file, "contents").expect("file");

        let error =
            EulerHome::resolve_from_env(Some(temp.path().into()), Some(file.into_os_string()))
                .expect_err("file override");

        assert!(matches!(error, EulerHomeError::NotDirectory { .. }));
    }

    #[test]
    fn ensure_creates_home_and_sessions_dirs() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join(".euler");

        let home = EulerHome::from_root(&root).expect("home");
        home.ensure().expect("ensure home");

        assert!(home.root().is_dir());
        assert!(home.sessions_dir().is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn directories_are_created_with_restrictive_permissions() {
        let temp = tempfile::tempdir().expect("temp dir");
        let home = EulerHome::from_root(temp.path().join(".euler")).expect("home");
        home.ensure().expect("ensure home");

        assert_eq!(mode(home.root()), 0o700);
        assert_eq!(mode(&home.sessions_dir()), 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_loop_euler_home_override() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temp dir");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        symlink(&second, &first).expect("first symlink");
        symlink(&first, &second).expect("second symlink");

        let error =
            EulerHome::resolve_from_env(None, Some(first.into_os_string())).expect_err("loop");

        assert!(matches!(error, EulerHomeError::SymlinkLoop { .. }));
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }
}
