//! Background jobs a sandboxed command left running, ended when the session
//! ends (ADR 0026 resolved questions 6 and 12).
//!
//! Seatbelt has no PID namespace, so a job that redirects its output
//! outlives its `run_shell` command. Bubblewrap runs each command in its own
//! PID namespace, which ends every job with the command, so nothing reaches
//! this list on Linux. A kept entry holds the command's process-group leader
//! exited but unreaped: the zombie pins its process-group id, so ending the
//! group later can never signal a reused group. A job that moves itself to
//! another process group (`setsid`) is outside this ownership, as it is for
//! cancellation.

use std::process::Child;
use std::sync::Mutex;

#[derive(Debug, Default)]
pub(crate) struct SandboxedJobs {
    leaders: Mutex<Vec<Child>>,
}

impl SandboxedJobs {
    /// Take the exited, unreaped `leader` of a finished sandboxed command.
    /// It is kept while other processes of its group still run; otherwise
    /// it is reaped now. Groups whose jobs have all exited since are reaped
    /// too, so the list does not grow with finished jobs.
    pub(crate) fn keep_or_reap(&self, mut leader: Child) {
        let mut leaders = self
            .leaders
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        leaders.retain_mut(|kept| {
            let live = group_has_other_members(kept.id());
            if !live {
                let _ = kept.wait();
            }
            live
        });
        if group_has_other_members(leader.id()) {
            leaders.push(leader);
        } else {
            let _ = leader.wait();
        }
    }

    /// End every kept job: kill each process group, then reap its leader.
    pub(crate) fn end_all(&self) {
        let mut leaders = self
            .leaders
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for mut leader in leaders.drain(..) {
            let group = leader.id() as libc::pid_t;
            // SAFETY: plain libc kill on a process group whose unreaped
            // leader pins the id, so the group cannot have been reused.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
            let _ = leader.wait();
        }
    }
}

impl Drop for SandboxedJobs {
    fn drop(&mut self) {
        self.end_all();
    }
}

/// Whether the process group led by `leader` has a member besides the
/// leader, which is a zombie by now.
#[cfg(target_os = "macos")]
fn group_has_other_members(leader: u32) -> bool {
    let leader = leader as libc::pid_t;
    let mut pids = vec![0 as libc::pid_t; 64];
    loop {
        let capacity = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: `pids` is a live buffer of `capacity` bytes for the call.
        // It returns the number of pids written, zombies included.
        let count = unsafe { libc::proc_listpgrppids(leader, pids.as_mut_ptr().cast(), capacity) };
        let Ok(count) = usize::try_from(count) else {
            return false;
        };
        if count < pids.len() {
            return pids[..count].iter().any(|pid| *pid != leader && *pid != 0);
        }
        pids.resize(pids.len() * 2, 0);
    }
}

/// Bubblewrap's PID namespace ends every job with its command.
#[cfg(not(target_os = "macos"))]
fn group_has_other_members(_leader: u32) -> bool {
    false
}

/// The exit code of `child` if it has exited, without reaping it, so its
/// process-group id stays pinned. `-1` for a signal death, as
/// `ExitStatus::code` reads it.
pub(crate) fn exit_code_without_reaping(child: &Child) -> std::io::Result<Option<i32>> {
    // SAFETY: an all-zero siginfo_t is a valid out-parameter for waitid.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: waits on our own child without reaping it (WNOWAIT) and
    // without blocking (WNOHANG); `info` outlives the call.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: waitid filled `info` for an exited child, or left it zeroed.
    let (pid, code, status) = unsafe { (info.si_pid(), info.si_code, info.si_status()) };
    if pid == 0 {
        return Ok(None);
    }
    Ok(Some(if code == libc::CLD_EXITED { status } else { -1 }))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn exited(child: &Child) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(code) = exit_code_without_reaping(child).expect("waitid") {
                return code;
            }
            assert!(Instant::now() < deadline, "leader did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn alive(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only probes for existence.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn a_job_left_in_the_group_is_kept_and_ended_with_the_session() {
        let jobs = SandboxedJobs::default();
        let leader = Command::new("/bin/sh")
            .args(["-c", "sleep 30 >/dev/null 2>&1 & echo $!; exit 3"])
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("spawn");
        let mut leader = leader;
        let mut stdout = String::new();
        std::io::Read::read_to_string(&mut leader.stdout.take().expect("stdout"), &mut stdout)
            .expect("job pid");
        let job: libc::pid_t = stdout.trim().parse().expect("pid");
        assert_eq!(exited(&leader), 3);
        jobs.keep_or_reap(leader);
        assert!(alive(job), "the job outlives its command");
        assert_eq!(jobs.leaders.lock().expect("lock").len(), 1);

        jobs.end_all();
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(job) {
            assert!(Instant::now() < deadline, "the job survived the session");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_command_with_no_job_is_reaped_at_once() {
        let jobs = SandboxedJobs::default();
        let leader = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .spawn()
            .expect("spawn");
        assert_eq!(exited(&leader), 0);
        jobs.keep_or_reap(leader);
        assert!(jobs.leaders.lock().expect("lock").is_empty());
    }
}
