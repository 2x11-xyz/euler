//! Background jobs a host `run_shell` command leaves running (ADR 0026
//! resolved questions 6 and 12).
//!
//! Each command runs in its own process group. When a host command's shell
//! exits while other members of that group still run (`server > log 2>&1
//! &`), the group is kept here with its leader exited but unreaped. An
//! unreaped leader pins its pid, and with it the group id, so the id cannot
//! be reused while Euler still lists the job. Under a sandbox-off profile
//! these jobs keep running, as in an unsandboxed local agent, and are listed
//! at exit. Jobs a sandboxed command leaves running are ended instead
//! (`sandboxed_jobs.rs`), which shares this module's process-group helpers.
//! A descendant that moves itself into another process group is outside
//! this tracking.

use std::process::Child;
use std::sync::{Mutex, MutexGuard};

/// Display bound for a job's command in the exit listing.
const COMMAND_DISPLAY_CHARS: usize = 80;

#[derive(Debug, Default)]
pub(crate) struct BackgroundJobs {
    jobs: Mutex<Vec<BackgroundJob>>,
}

#[derive(Debug)]
struct BackgroundJob {
    /// Exited and unreaped; its pid is the group id.
    leader: Child,
    command: String,
}

impl BackgroundJob {
    fn group(&self) -> i32 {
        self.leader.id() as i32
    }

    /// Reap the leader without signalling anyone. After this the group id
    /// is no longer pinned.
    fn release(mut self) {
        let _ = self.leader.wait();
    }
}

impl BackgroundJobs {
    /// Keep the group of a host command whose leader exited while other
    /// members still run.
    pub(crate) fn track(&self, leader: Child, command: &str) {
        let mut jobs = self.lock();
        release_finished(&mut jobs);
        jobs.push(BackgroundJob {
            leader,
            command: command.to_owned(),
        });
    }

    /// Move every job to `to`, whose session continues this one in the same
    /// process (`/new`).
    pub(crate) fn hand_over(&self, to: &BackgroundJobs) {
        let jobs: Vec<_> = self.lock().drain(..).collect();
        to.lock().extend(jobs);
    }

    /// The jobs that still run, one display line each.
    pub(crate) fn running(&self) -> Vec<String> {
        let mut jobs = self.lock();
        release_finished(&mut jobs);
        jobs.iter()
            .map(|job| {
                format!(
                    "process group {}: {}",
                    job.group(),
                    command_display(&job.command)
                )
            })
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, Vec<BackgroundJob>> {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for BackgroundJobs {
    /// Session end: the jobs keep running; their leaders are only reaped.
    fn drop(&mut self) {
        self.lock().drain(..).for_each(BackgroundJob::release);
    }
}

/// Reap jobs whose group has no other running member left.
fn release_finished(jobs: &mut Vec<BackgroundJob>) {
    let (running, finished): (Vec<_>, Vec<_>) = jobs
        .drain(..)
        .partition(|job| group_has_other_members(job.group()));
    *jobs = running;
    finished.into_iter().for_each(BackgroundJob::release);
}

fn command_display(command: &str) -> String {
    let first_line = command.lines().next().unwrap_or_default();
    let mut display: String = first_line
        .chars()
        .take(COMMAND_DISPLAY_CHARS)
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect();
    if first_line.chars().count() > COMMAND_DISPLAY_CHARS || command.lines().nth(1).is_some() {
        display.push_str("...");
    }
    display
}

/// The exit code of child `pid` if it has exited, leaving it unreaped. A
/// signal-terminated child reads as -1, as an exit status without a code
/// does elsewhere.
pub(crate) fn exit_code_without_reaping(pid: i32) -> std::io::Result<Option<i32>> {
    // SAFETY: `siginfo_t` is plain data; all-zero is a valid value and lets
    // a no-state-change result read as `si_pid == 0`.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid, writable `siginfo_t`. `WNOWAIT` leaves the
    // child waitable, so the caller still owns reaping it.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            return Ok(None);
        }
        return Err(error);
    }
    // SAFETY: `waitid` filled `info` (or left it zeroed for no change).
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    Ok(Some(if info.si_code == libc::CLD_EXITED {
        // SAFETY: as above.
        unsafe { info.si_status() }
    } else {
        -1
    }))
}

/// Whether process group `group` has a running member other than its leader.
#[cfg(target_os = "linux")]
pub(crate) fn group_has_other_members(group: i32) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    entries.flatten().any(|entry| {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            return false;
        };
        if pid == group {
            return false;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            return false;
        };
        // `pid (comm) state ppid pgrp ...`; `comm` may contain spaces and
        // parentheses, so fields are read after the last `)`.
        let Some((_, fields)) = stat.rsplit_once(')') else {
            return false;
        };
        let mut fields = fields.split_whitespace();
        let state = fields.next();
        let pgrp = fields.nth(1).and_then(|field| field.parse::<i32>().ok());
        state != Some("Z") && pgrp == Some(group)
    })
}

/// Whether process group `group` has a member other than its leader. The
/// listing includes zombies, so a member that just exited still counts
/// until it is reaped; that only keeps a job tracked a little longer.
#[cfg(target_os = "macos")]
pub(crate) fn group_has_other_members(group: i32) -> bool {
    let mut pids = vec![0 as libc::pid_t; 64];
    loop {
        let Ok(capacity) = libc::c_int::try_from(std::mem::size_of_val(pids.as_slice())) else {
            return false;
        };
        // SAFETY: `pids` is a live, writable buffer of `capacity` bytes for
        // the call, which returns the number of pids written.
        let count = unsafe { libc::proc_listpgrppids(group, pids.as_mut_ptr().cast(), capacity) };
        let Ok(count) = usize::try_from(count) else {
            return false;
        };
        // A full buffer may have been truncated: grow it and list again.
        if count < pids.len() {
            return pids[..count].iter().any(|&pid| pid > 0 && pid != group);
        }
        pids.resize(pids.len() * 2, 0);
    }
}

/// No process listing on this platform, so nothing is tracked.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn group_has_other_members(_group: i32) -> bool {
    false
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt as _;
    use std::time::{Duration, Instant};

    /// A process group whose shell exited while a background member keeps
    /// running, returned with the leader unreaped.
    fn group_with_live_member() -> Child {
        let child = std::process::Command::new("sh")
            .args(["-c", "sleep 30 >/dev/null 2>&1 & exit 0"])
            .process_group(0)
            .spawn()
            .expect("spawn");
        let group = child.id() as i32;
        wait_until(|| {
            exit_code_without_reaping(group).expect("waitid") == Some(0)
                && group_has_other_members(group)
        });
        child
    }

    fn wait_until(condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "condition not reached");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn host_jobs_keep_running_are_listed_and_move_to_a_fresh_session() {
        let jobs = BackgroundJobs::default();
        let host = group_with_live_member();
        let host_group = host.id() as i32;
        jobs.track(host, "sleep 30 &\nsecond line");

        assert!(group_has_other_members(host_group));
        assert_eq!(
            jobs.running(),
            vec![format!("process group {host_group}: sleep 30 &...")]
        );
        // `/new` hands the job to the fresh session's registry.
        let fresh = BackgroundJobs::default();
        jobs.hand_over(&fresh);
        assert!(jobs.running().is_empty());
        assert_eq!(fresh.running().len(), 1);
        // SAFETY: the tracked, unreaped leader still pins this group id.
        unsafe {
            libc::kill(-host_group, libc::SIGKILL);
        }
        wait_until(|| !group_has_other_members(host_group));
        assert!(fresh.running().is_empty());
    }
}
