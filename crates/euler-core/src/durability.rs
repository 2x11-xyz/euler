//! Durability primitives shared by the state writers (grants, swarm config,
//! session store, provenance, extension registry, auth storage): directory
//! fsyncs and file syncs, routed through a test-only fault-injection seam so
//! the error paths are testable. Outside `cfg(test)` the seam's check is an
//! `#[inline(always)]` `Ok(())`, so release builds compile down to the direct
//! `sync_data`/`sync_all` calls.
//!
//! Background: the swallowed-fsync bug class (directory syncs silently
//! dropped by grants/swarm writers, fixed in PR #194) was structurally
//! untestable because no harness could make `sync_data`/`sync_dir` fail.

use std::fs::File;
use std::io;
use std::path::Path;

/// fsync the directory at `path` so a preceding create/rename within it is
/// durable.
#[cfg(unix)]
pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    fault::check(fault::Op::DirSync, path)?;
    File::open(path)?.sync_all()
}

/// fsync the directory at `path` so a preceding create/rename within it is
/// durable.
#[cfg(not(unix))]
pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    // std exposes no portable directory fsync on non-Unix platforms.
    fault::check(fault::Op::DirSync, path)
}

/// [`File::sync_data`] routed through the fault-injection seam. `path` is
/// the file's own path, used only to match armed faults.
pub(crate) fn sync_file_data(file: &File, path: &Path) -> io::Result<()> {
    fault::check(fault::Op::FileSync, path)?;
    file.sync_data()
}

/// [`File::sync_all`] routed through the fault-injection seam. `path` is
/// the file's own path, used only to match armed faults.
pub(crate) fn sync_file_all(file: &File, path: &Path) -> io::Result<()> {
    fault::check(fault::Op::FileSync, path)?;
    file.sync_all()
}

/// No-op fault seam for non-test builds: `check` inlines to `Ok(())`.
#[cfg(not(test))]
pub(crate) mod fault {
    use std::io;
    use std::path::Path;

    #[derive(Clone, Copy)]
    pub(crate) enum Op {
        DirSync,
        FileSync,
    }

    #[inline(always)]
    pub(crate) fn check(_op: Op, _path: &Path) -> io::Result<()> {
        Ok(())
    }
}

/// Test-only fault injection: a thread-local, one-shot fault armed against
/// the sync primitives above. Writers under test run their syncs on the
/// calling thread, so arming on the test thread is sufficient, and the
/// thread-local keeps concurrently running tests isolated.
#[cfg(test)]
pub(crate) mod fault {
    use std::cell::RefCell;
    use std::io;
    use std::path::Path;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum Op {
        DirSync,
        FileSync,
    }

    struct Armed {
        op: Op,
        matcher: Box<dyn Fn(&Path) -> bool>,
        fired: bool,
    }

    thread_local! {
        static ARMED: RefCell<Option<Armed>> = const { RefCell::new(None) };
    }

    /// Arm a one-shot fault on the current thread: the first `op` call whose
    /// path satisfies `matcher` fails with an injected [`io::Error`]. The
    /// fault disarms when it fires or when the returned guard drops; assert
    /// [`FaultGuard::fired`] so a test cannot pass vacuously.
    pub(crate) fn arm_matching(op: Op, matcher: impl Fn(&Path) -> bool + 'static) -> FaultGuard {
        ARMED.with(|cell| {
            let mut slot = cell.borrow_mut();
            assert!(
                slot.is_none(),
                "a durability fault is already armed on this thread"
            );
            *slot = Some(Armed {
                op,
                matcher: Box::new(matcher),
                fired: false,
            });
        });
        FaultGuard(())
    }

    /// Disarms the fault (if unfired) when dropped.
    pub(crate) struct FaultGuard(());

    impl FaultGuard {
        /// Whether the armed fault actually failed a sync call.
        pub(crate) fn fired(&self) -> bool {
            ARMED.with(|cell| cell.borrow().as_ref().is_some_and(|armed| armed.fired))
        }
    }

    impl Drop for FaultGuard {
        fn drop(&mut self) {
            ARMED.with(|cell| cell.borrow_mut().take());
        }
    }

    pub(crate) fn check(op: Op, path: &Path) -> io::Result<()> {
        ARMED.with(|cell| {
            let mut slot = cell.borrow_mut();
            let Some(armed) = slot.as_mut() else {
                return Ok(());
            };
            if armed.fired || armed.op != op || !(armed.matcher)(path) {
                return Ok(());
            }
            armed.fired = true;
            Err(io::Error::other(format!(
                "injected {op:?} fault at {}",
                path.display()
            )))
        })
    }
}
