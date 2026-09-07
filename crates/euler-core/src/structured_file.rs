//! Descriptor-anchored opens for the structured file tools.
//!
//! Every structured read and write resolves its target by walking down from
//! an already-authorized workspace root, never by handing a joined path to
//! the kernel. A same-user host actor (or the agent's own `run_shell`) can
//! replace any path component between the moment a path is resolved and the
//! moment it is opened; only an fd-anchored walk closes that window.
//!
//! * Linux uses one `openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS |
//!   RESOLVE_NO_XDEV | RESOLVE_NO_MAGICLINKS`. `RESOLVE_NO_XDEV` uses mount
//!   identity, so a same-device bind mount planted inside the workspace is
//!   rejected as firmly as a different filesystem.
//! * Other Unix hosts walk hop by hop with `O_DIRECTORY | O_NOFOLLOW |
//!   O_CLOEXEC`, which makes each directory component its own boundary.
//! * Non-Unix hosts fall back to an ordinary open; the structured tools still
//!   apply the regular-file and link-count checks in `tools.rs`.

use std::fs;
use std::path::Path;

#[cfg(unix)]
use std::path::Component;

/// Reject anything that is not a normalized, relative, non-empty path before
/// it reaches the kernel. `..`, a leading `/`, and an empty path are all
/// resolution ambiguities the confined open must never have to reason about.
#[cfg(unix)]
fn checked_relative(relative: &Path) -> std::io::Result<()> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "structured path must be normalized and relative",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
const RESOLVE_NO_XDEV: u64 = 0x01;
#[cfg(target_os = "linux")]
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
#[cfg(target_os = "linux")]
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
#[cfg(target_os = "linux")]
const RESOLVE_BENEATH: u64 = 0x08;

#[cfg(target_os = "linux")]
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// Open a structured-tool file beneath an already-authorized root without
/// crossing a mount or following a raced symlink.
#[cfg(target_os = "linux")]
fn open_beneath(
    root: &Path,
    relative: &Path,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> std::io::Result<fs::File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;

    checked_relative(relative)?;
    let root = std::ffi::CString::new(root.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let relative = std::ffi::CString::new(relative.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: `root` is NUL-terminated and the returned descriptor is either
    // negative or uniquely owned and immediately wrapped in `File`.
    let root_fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if root_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `open` returned a fresh descriptor owned by this function.
    let root_file = unsafe { fs::File::from_raw_fd(root_fd) };
    let flags = u64::try_from(flags | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let how = OpenHow {
        flags,
        mode: u64::from(mode),
        resolve: RESOLVE_BENEATH | RESOLVE_NO_XDEV | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS,
    };
    // SAFETY: both path strings are NUL-terminated, `root_file` remains live,
    // and `how` is the kernel's three-u64 `open_how` layout for this size.
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root_file.as_raw_fd(),
            relative.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let descriptor = libc::c_int::try_from(descriptor)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
    // SAFETY: `openat2` returned a fresh descriptor owned by the result.
    Ok(unsafe { fs::File::from_raw_fd(descriptor) })
}

/// One descriptor-anchored component walker for Unix hosts without
/// `openat2`. `O_NOFOLLOW` on every hop makes each directory component an
/// fd-anchored boundary, so a component swapped for a symlink after path
/// resolution fails the open instead of escaping the root.
#[cfg(all(unix, not(target_os = "linux")))]
fn open_beneath(
    root: &Path,
    relative: &Path,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> std::io::Result<fs::File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;

    checked_relative(relative)?;
    let root_name = std::ffi::CString::new(root.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: `root_name` is NUL-terminated and the returned descriptor is
    // either negative or uniquely owned and immediately wrapped in `File`.
    let root_fd = unsafe {
        libc::open(
            root_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if root_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `open` returned a fresh descriptor owned by this function.
    let mut directory = unsafe { fs::File::from_raw_fd(root_fd) };
    let components = relative.components().collect::<Vec<_>>();
    let (file_name, parents) = components
        .split_last()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;

    for component in parents {
        let component = std::ffi::CString::new(component.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        // SAFETY: the live directory fd and NUL-terminated component are
        // valid. O_NOFOLLOW makes each directory hop an fd-anchored boundary.
        let next_fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if next_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `openat` returned a fresh descriptor owned by `next`.
        directory = unsafe { fs::File::from_raw_fd(next_fd) };
    }

    let file_name = std::ffi::CString::new(file_name.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let flags = flags | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    // SAFETY: the parent fd and final NUL-terminated name are valid. The mode
    // is consulted only when the caller passed O_CREAT.
    let file_fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            file_name.as_ptr(),
            flags,
            libc::c_uint::from(mode),
        )
    };
    if file_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `openat` returned a fresh descriptor owned by `file`.
    Ok(unsafe { fs::File::from_raw_fd(file_fd) })
}

/// Open an existing structured-tool file for reading, confined to `root`.
#[cfg(unix)]
pub(crate) fn open_read(root: &Path, relative: &Path) -> std::io::Result<fs::File> {
    // O_NONBLOCK keeps a FIFO planted at the target from blocking the open
    // until a writer appears; the caller still rejects it as a non-regular
    // file.
    open_beneath(root, relative, libc::O_RDONLY | libc::O_NONBLOCK, 0)
}

/// Open a structured-tool file for writing, confined to `root`. `create_new`
/// requires `O_EXCL`, so an add can never silently clobber a file (or a
/// symlink) that appeared after the tool call was prepared.
#[cfg(unix)]
pub(crate) fn open_write(
    root: &Path,
    relative: &Path,
    create_new: bool,
) -> std::io::Result<fs::File> {
    let mut flags = libc::O_RDWR | libc::O_NONBLOCK;
    if create_new {
        flags |= libc::O_CREAT | libc::O_EXCL;
    }
    let mode = if create_new { 0o666 } else { 0 };
    open_beneath(root, relative, flags, mode)
}

#[cfg(not(unix))]
pub(crate) fn open_read(root: &Path, relative: &Path) -> std::io::Result<fs::File> {
    fs::File::open(root.join(relative))
}

#[cfg(not(unix))]
pub(crate) fn open_write(
    root: &Path,
    relative: &Path,
    create_new: bool,
) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true);
    if create_new {
        options.create_new(true);
    }
    options.open(root.join(relative))
}

/// Number of directory entries naming this inode. `> 1` means a second name
/// exists somewhere on the filesystem, possibly outside the workspace.
#[cfg(unix)]
pub(crate) fn hardlink_count(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;

    metadata.nlink()
}

#[cfg(not(unix))]
pub(crate) fn hardlink_count(_metadata: &fs::Metadata) -> u64 {
    1
}
