//! Descriptor-anchored opens and atomic replacement for the structured file
//! tools.
//!
//! Every structured read and write resolves its target by walking down from
//! an already-authorized workspace root, never by handing a joined path to
//! the kernel. A same-user host actor (or the agent's own `run_shell`) can
//! replace any path component between the moment a path is resolved and the
//! moment it is opened; only an fd-anchored walk closes that window.
//!
//! Resolution stops at the target's *parent* directory. Holding that
//! descriptor is what makes the write atomic: the new bytes go to a temporary
//! file created in the same directory and are then `renameat`d over the
//! target name, so the target is only ever its complete old content or its
//! complete new content, never a truncated intermediate.
//!
//! * Linux resolves the parent with one `openat2` using `RESOLVE_BENEATH |
//!   RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`, falling back to the
//!   hop-by-hop walker when the syscall is unavailable (kernels before 5.6,
//!   or a seccomp profile that denies it).
//! * Other Unix hosts walk hop by hop with `O_DIRECTORY | O_NOFOLLOW |
//!   O_CLOEXEC`, which makes each directory component its own boundary.
//! * Euler ships on Linux and macOS. There is no confined open for other
//!   targets, so the structured tools fail closed there rather than quietly
//!   opening a joined path.
//!
//! A crash between creating the temporary file and renaming it can leave a
//! `.euler-write-<id>.tmp` sibling. Nothing sweeps them: enumerating the
//! directory by path would break the fd-anchored rule, and unlinking by name
//! could delete a concurrent Euler's in-flight temporary. Workspace
//! observation ignores the name shape instead, so a leftover is never
//! reported as a change; removing it is the user's, or the OS temp
//! reaper's, business.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use std::ffi::{CString, OsStr};
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;

/// A structured-tool target reduced to a descriptor on its confined parent
/// directory plus the final component name. Every operation on the target
/// goes through that descriptor, so no later path lookup can be redirected.
pub(crate) struct ConfinedTarget {
    #[cfg(unix)]
    directory: OwnedFd,
    #[cfg(unix)]
    name: OsString,
    absolute: PathBuf,
}

/// Resolve `relative` beneath `root` to a descriptor on its parent directory.
pub(crate) fn confine(root: &Path, relative: &Path) -> io::Result<ConfinedTarget> {
    let components = normalized_components(relative)?;
    let (name, parents) = components
        .split_last()
        .ok_or_else(|| invalid("structured path must name a file"))?;
    let absolute = root.join(relative);
    #[cfg(unix)]
    {
        Ok(ConfinedTarget {
            directory: open_parent(root, parents)?,
            name: name.clone(),
            absolute,
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (parents, name, absolute);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "structured file tools require a Unix host",
        ))
    }
}

/// Reject anything that is not a normalized, relative, non-empty path before
/// it reaches the kernel. `..`, a leading `/`, and an empty path are all
/// resolution ambiguities the confined open must never have to reason about.
fn normalized_components(relative: &Path) -> io::Result<Vec<OsString>> {
    if relative.as_os_str().is_empty() {
        return Err(invalid("structured path must not be empty"));
    }
    relative
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name.to_os_string()),
            _ => Err(invalid("structured path must be normalized and relative")),
        })
        .collect()
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(unix)]
impl ConfinedTarget {
    /// Open the existing target for reading.
    ///
    /// `O_NONBLOCK` keeps a FIFO planted at the target from blocking the open
    /// until a writer appears; the caller still rejects it as a non-regular
    /// file.
    pub(crate) fn open_read(&self) -> io::Result<fs::File> {
        openat_in(
            &self.directory,
            &self.name,
            libc::O_RDONLY | libc::O_NONBLOCK,
            0,
        )
        .map(fs::File::from)
    }

    /// Create the target atomically, failing if any name already exists.
    ///
    /// The content is written to a sibling temporary and published with a
    /// no-replace rename, so a create is all-or-nothing for the same reason a
    /// replace is: a SIGKILL mid-fill cannot leave a partial file under the
    /// real name — which would also have blocked every retry, since the
    /// no-clobber rule would then refuse the name it left behind.
    pub(crate) fn create_new(&self, content: &[u8]) -> io::Result<Durability> {
        self.publish(content, None, true)
    }

    /// Replace the target's content atomically: write `content` to a fresh
    /// file in the same confined directory, give it the old file's mode and
    /// ownership, make it durable, then rename it over the target name.
    ///
    /// A crash at any point leaves the target as either its complete old
    /// content or its complete new content.
    pub(crate) fn replace(
        &self,
        content: &[u8],
        previous: &fs::Metadata,
    ) -> io::Result<Durability> {
        self.publish(content, Some(previous), false)
    }

    /// Write `content` to a sibling temporary and rename it onto the target
    /// name. `no_replace` makes the rename refuse an existing name instead of
    /// overwriting it, which is what keeps a create no-clobber.
    fn publish(
        &self,
        content: &[u8],
        previous: Option<&fs::Metadata>,
        no_replace: bool,
    ) -> io::Result<Durability> {
        let temp_name = OsString::from(format!(
            "{}{}{}",
            crate::file_diff::STRUCTURED_WRITE_TEMP_PREFIX,
            ulid::Ulid::new(),
            crate::file_diff::STRUCTURED_WRITE_TEMP_SUFFIX
        ));
        let temp = openat_in(
            &self.directory,
            &temp_name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            if previous.is_some() { 0o600 } else { 0o666 },
        )?;
        let written = previous
            .map_or(Ok(()), |previous| self.inherit_identity(&temp, previous))
            .and_then(|()| self.fill(temp, &temp_name, content));
        let published = written.and_then(|()| self.rename_over(&temp_name, no_replace));
        if let Err(error) = published {
            let _ = unlinkat(&self.directory, &temp_name);
            return Err(error);
        }
        // Past this point the new content is published: the rename cannot be
        // undone and the caller must not be told the write failed.
        Ok(self.sync_directory())
    }

    /// Whether this process can actually replace the target.
    ///
    /// A rename-over publishes a new directory entry, so the kernel checks
    /// the *directory*, not the file: without this, `chmod a-w generated.rs`
    /// would silently stop protecting the file, and a target in a read-only
    /// directory would fail with a bare `EACCES` after a checkpoint had
    /// already been recorded.
    pub(crate) fn writability(&self) -> io::Result<Writability> {
        if !self.access(Some(&self.name), libc::W_OK)? {
            return Ok(Writability::ReadOnlyFile);
        }
        if !self.access(None, libc::W_OK)? {
            return Ok(Writability::ReadOnlyDirectory);
        }
        Ok(Writability::Writable)
    }

    /// `faccessat` against the held directory, using the real (not effective)
    /// answer the kernel would give this process for a write.
    fn access(&self, name: Option<&OsStr>, mode: libc::c_int) -> io::Result<bool> {
        let name = match name {
            Some(name) => c_name(name)?,
            // An empty relative path with AT_EMPTY_PATH semantics is not
            // portable; "." names the directory the descriptor holds.
            None => c_name(OsStr::new("."))?,
        };
        // SAFETY: the directory descriptor is live and the name is
        // NUL-terminated.
        let allowed = unsafe {
            libc::faccessat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                mode,
                libc::AT_EACCESS,
            )
        };
        if allowed == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EACCES) | Some(libc::EPERM) | Some(libc::EROFS) => Ok(false),
            // A missing target is not a permission answer; the caller's own
            // open decides.
            Some(libc::ENOENT) => Ok(true),
            _ => Err(error),
        }
    }

    /// Give the replacement the file it replaces. Permission bits are masked
    /// to `0o777`: an atomic replace must never carry setuid, setgid, or the
    /// sticky bit onto content the agent wrote. Ownership is best-effort —
    /// only a root-owned Euler can set it, and `EPERM` is the ordinary case.
    fn inherit_identity(&self, temp: &OwnedFd, previous: &fs::Metadata) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;

        let mode = previous.permissions().mode() & 0o777;
        // SAFETY: the borrowed descriptor is live for the duration of the call.
        if unsafe { libc::fchmod(temp.as_raw_fd(), mode as libc::mode_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: same live descriptor; a refusal is expected and ignored.
        unsafe { libc::fchown(temp.as_raw_fd(), previous.uid(), previous.gid()) };
        Ok(())
    }

    fn fill(&self, file: OwnedFd, name: &OsStr, content: &[u8]) -> io::Result<()> {
        use std::io::Write as _;

        let mut file = fs::File::from(file);
        file.write_all(content)?;
        // The bytes must be on disk before the name that publishes them.
        crate::durability::sync_file_data(&file, &self.absolute.with_file_name(name))
    }

    fn rename_over(&self, temp_name: &OsStr, no_replace: bool) -> io::Result<()> {
        let from = c_name(temp_name)?;
        let to = c_name(&self.name)?;
        let from_fd = self.directory.as_raw_fd();
        let result = if no_replace {
            rename_no_replace(from_fd, &from, &to)
        } else {
            // SAFETY: both names are NUL-terminated single components and
            // the directory descriptor is live for the duration of the call.
            unsafe { libc::renameat(from_fd, from.as_ptr(), from_fd, to.as_ptr()) }
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            // The name appeared after this call was prepared. Report it the
            // way an `O_EXCL` create would.
            return Err(if error.raw_os_error() == Some(libc::EEXIST) {
                io::Error::from(io::ErrorKind::AlreadyExists)
            } else {
                error
            });
        }
        Ok(())
    }

    /// Make the new directory entry durable through the descriptor the write
    /// used, never by re-opening the path. A failure here does not undo the
    /// write, so it is reported as a durability caveat rather than an error.
    fn sync_directory(&self) -> Durability {
        use std::os::fd::AsFd as _;

        let parent = self.absolute.parent().unwrap_or(&self.absolute);
        match crate::durability::sync_directory_fd(self.directory.as_fd(), parent) {
            Ok(()) => Durability::Synced,
            Err(error) => Durability::DirectoryUnsynced(error.to_string()),
        }
    }
}

/// Whether a structured write is permitted to replace its target.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Writability {
    Writable,
    /// The file itself is not writable by this process. A rename-over would
    /// ignore that, so it is refused instead.
    ReadOnlyFile,
    /// The directory is not writable, so no new entry can be published.
    ReadOnlyDirectory,
}

/// Whether the published write reached the disk. The content itself is
/// always durable before it is published; only the directory entry can be
/// left unsynced, and that cannot un-publish the write.
#[derive(Debug)]
pub(crate) enum Durability {
    Synced,
    DirectoryUnsynced(String),
}

/// Rename that refuses an existing destination, so a create cannot clobber a
/// name that appeared after the call was prepared. Both supported targets
/// have a flag for it; nothing else is reachable here.
#[cfg(target_os = "linux")]
fn rename_no_replace(directory: libc::c_int, from: &CString, to: &CString) -> libc::c_int {
    const RENAME_NOREPLACE: libc::c_uint = 1;

    // SAFETY: both names are NUL-terminated single components and the
    // directory descriptor is live for the duration of the call.
    unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            directory,
            from.as_ptr(),
            directory,
            to.as_ptr(),
            RENAME_NOREPLACE,
        ) as libc::c_int
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn rename_no_replace(directory: libc::c_int, from: &CString, to: &CString) -> libc::c_int {
    const RENAME_EXCL: libc::c_uint = 0x0000_0004;

    // SAFETY: both names are NUL-terminated single components and the
    // directory descriptor is live for the duration of the call.
    unsafe {
        libc::renameatx_np(
            directory,
            from.as_ptr(),
            directory,
            to.as_ptr(),
            RENAME_EXCL,
        )
    }
}

#[cfg(unix)]
fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| invalid("structured path component contains NUL"))
}

/// `openat` one single component with no-follow, close-on-exec semantics.
#[cfg(unix)]
fn openat_in(
    directory: &OwnedFd,
    name: &OsStr,
    flags: libc::c_int,
    mode: u32,
) -> io::Result<OwnedFd> {
    let name = c_name(name)?;
    // SAFETY: the directory descriptor is live, the name is NUL-terminated,
    // and the returned descriptor is either negative or uniquely owned.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            libc::c_uint::from(mode),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openat` returned a fresh descriptor owned here.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(unix)]
fn unlinkat(directory: &OwnedFd, name: &OsStr) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: the directory descriptor is live and the name is NUL-terminated.
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Flags for a held directory descriptor.
///
/// `O_PATH` would be enough to anchor `*at` calls, but `fsync` on an `O_PATH`
/// descriptor fails with `EBADF`, and a published write must be able to make
/// its directory entry durable through the very descriptor it wrote through.
const DIRECTORY_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY;

/// Open the workspace root itself. This is the one absolute-path open; the
/// root is the authority the rest of the walk anchors to.
#[cfg(unix)]
fn open_root(root: &Path) -> io::Result<OwnedFd> {
    let root = c_name(root.as_os_str())?;
    // SAFETY: `root` is NUL-terminated and the returned descriptor is either
    // negative or uniquely owned.
    let fd = unsafe {
        libc::open(
            root.as_ptr(),
            DIRECTORY_FLAGS | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `open` returned a fresh descriptor owned here.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Walk the parent components hop by hop. `O_NOFOLLOW` on every hop makes
/// each directory component an fd-anchored boundary, so a component swapped
/// for a symlink after path resolution fails the walk instead of escaping it.
#[cfg(unix)]
fn walk_parents(root: &Path, parents: &[OsString]) -> io::Result<OwnedFd> {
    let mut directory = open_root(root)?;
    for component in parents {
        directory = openat_in(&directory, component, DIRECTORY_FLAGS, 0)?;
    }
    Ok(directory)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn open_parent(root: &Path, parents: &[OsString]) -> io::Result<OwnedFd> {
    walk_parents(root, parents)
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// Whether this kernel refused `openat2`. Set once and read thereafter, so a
/// host without the syscall pays for one failed call rather than one per tool.
#[cfg(target_os = "linux")]
static OPENAT2_UNAVAILABLE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Resolve the parent directory in one confined syscall.
///
/// `RESOLVE_NO_XDEV` is deliberately not requested. It rejects every mount
/// crossing, which breaks ordinary setups — a devcontainer volume mounted at
/// `node_modules/`, a tmpfs at `target/` — while the escape it would prevent
/// needs privileges the same-user threat model already excludes.
#[cfg(target_os = "linux")]
fn open_parent(root: &Path, parents: &[OsString]) -> io::Result<OwnedFd> {
    use std::sync::atomic::Ordering;

    if parents.is_empty() {
        return open_root(root);
    }
    if OPENAT2_UNAVAILABLE.load(Ordering::Relaxed) {
        return walk_parents(root, parents);
    }
    let root_fd = open_root(root)?;
    let relative = parents.iter().collect::<PathBuf>();
    let relative = c_name(relative.as_os_str())?;
    let how = OpenHow {
        flags: u64::try_from(DIRECTORY_FLAGS | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .map_err(|_| invalid("unrepresentable open flags"))?,
        mode: 0,
        resolve: libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_NO_MAGICLINKS,
    };
    // SAFETY: the root descriptor is live, `relative` is NUL-terminated, and
    // `how` is the kernel's three-u64 `open_how` layout for this size.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root_fd.as_raw_fd(),
            relative.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        // ENOSYS is a kernel older than 5.6; EPERM is a seccomp profile that
        // denies the syscall. Both mean "resolve it the other way", not
        // "refuse every structured tool".
        if matches!(error.raw_os_error(), Some(libc::ENOSYS) | Some(libc::EPERM)) {
            OPENAT2_UNAVAILABLE.store(true, Ordering::Relaxed);
            return walk_parents(root, parents);
        }
        return Err(error);
    }
    let fd = libc::c_int::try_from(fd).map_err(|_| invalid("unrepresentable descriptor"))?;
    // SAFETY: `openat2` returned a fresh descriptor owned here.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) mod test_support {
    use std::sync::atomic::Ordering;

    /// Force the hop-by-hop walker for the lifetime of the guard, the way a
    /// kernel or seccomp profile without `openat2` would.
    pub(crate) struct WithoutOpenat2(bool);

    impl WithoutOpenat2 {
        pub(crate) fn arm() -> Self {
            Self(super::OPENAT2_UNAVAILABLE.swap(true, Ordering::Relaxed))
        }
    }

    impl Drop for WithoutOpenat2 {
        fn drop(&mut self) {
            super::OPENAT2_UNAVAILABLE.store(self.0, Ordering::Relaxed);
        }
    }
}
