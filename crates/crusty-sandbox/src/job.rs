//! Windows job object that terminates a command's whole process tree when dropped.

use std::io;
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::process::Child;

use win32job::{ExtendedLimitInfo, Job};

/// Owns a job object configured with kill-on-close. Processes assigned to it are terminated
/// when the guard is dropped, including their descendants.
pub struct JobGuard {
    job: Job,
}

impl JobGuard {
    /// Creates a job whose processes are killed when the job handle closes.
    pub fn new() -> io::Result<Self> {
        let mut info = ExtendedLimitInfo::new();
        info.limit_kill_on_job_close();
        let job = Job::create_with_limit_info(&info).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(JobGuard { job })
    }

    /// Puts a spawned child into the job. Call it right after spawning, before the child
    /// starts its own children, so none escape the job.
    pub fn assign(&self, child: &Child) -> io::Result<()> {
        self.assign_handle(child.as_raw_handle())
    }

    /// Creates a kill-on-close job and puts the current process into it. Children started
    /// afterwards inherit the job; keep the guard alive until they have finished.
    pub fn join_current_process() -> io::Result<Self> {
        let guard = Self::new()?;
        guard.job.assign_current_process().map_err(|e| io::Error::other(e.to_string()))?;
        Ok(guard)
    }

    /// Puts the process behind `handle` into the job. For a tokio child, pass its `raw_handle()`.
    pub fn assign_handle(&self, handle: RawHandle) -> io::Result<()> {
        self.job.assign_process(handle as isize).map_err(|e| io::Error::other(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::JobGuard;

    #[test]
    fn dropping_guard_kills_the_child() {
        let guard = JobGuard::new().expect("job");
        let mut child = Command::new("cmd")
            .args(["/C", "ping", "-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        guard.assign(&child).expect("assign");
        let started = Instant::now();
        drop(guard);
        // Kill-on-close exits with code 0, so the exit code proves nothing: the early return does.
        child.wait().expect("wait");
        assert!(started.elapsed() < Duration::from_secs(20));
    }
}
