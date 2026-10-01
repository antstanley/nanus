//! Job Object ownership stays here; spawning is suspended until assignment by process-wrap.

use std::process::ExitStatus;
use std::sync::{Mutex, MutexGuard, PoisonError};

use process_wrap::tokio::{ChildWrapper, CommandWrap, JobObject, KillOnDrop};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use winsafe::{HPROCESS, co};

/// A failure to spawn, inspect, or terminate a Windows job.
#[derive(Debug, thiserror::Error)]
#[error("the Windows job failed: {0}")]
pub struct JobError(#[from] std::io::Error);

/// A process tree whose last owner closes its kill-on-close Job Object.
#[derive(Debug)]
pub struct Job {
    child: Mutex<Box<dyn ChildWrapper>>,
    pid: u32,
}

impl Job {
    /// Spawns into a Job Object before allowing the child's first instruction to run.
    pub fn spawn(command: Command) -> Result<Self, JobError> {
        let mut command = CommandWrap::from(command);
        let child = command.wrap(JobObject).wrap(KillOnDrop).spawn()?;
        let pid = child
            .id()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| std::io::Error::other("the spawned child has no valid process id"))?;
        assert!(pid > 0, "a job owns a valid process");
        Ok(Self {
            child: Mutex::new(child),
            pid,
        })
    }

    /// Returns the process id, never a raw handle.
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Takes the child's standard input once.
    pub fn take_stdin(&self) -> Option<ChildStdin> {
        self.lock().stdin().take()
    }
    /// Takes the child's standard output once.
    pub fn take_stdout(&self) -> Option<ChildStdout> {
        self.lock().stdout().take()
    }
    /// Takes the child's standard error once.
    pub fn take_stderr(&self) -> Option<ChildStderr> {
        self.lock().stderr().take()
    }

    /// Checks and reaps the leader without keeping a lock across an await.
    pub fn try_wait(&self) -> Result<Option<ExitStatus>, JobError> {
        // The leader's exit ends a tool; lingering descendants are killed by the owner.
        Ok(self.lock().inner_mut().try_wait()?)
    }

    /// Terminates every process in the Job Object.
    pub fn terminate(&self) -> Result<(), JobError> {
        Ok(self.lock().start_kill()?)
    }

    fn lock(&self) -> MutexGuard<'_, Box<dyn ChildWrapper>> {
        self.child.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Checks whether a process still exists, for verifying Job Object cleanup.
/// An inaccessible process is an error, never mistaken for a dead one.
pub fn is_process_alive(pid: u32) -> Result<bool, JobError> {
    assert!(pid > 0, "a process id is positive");
    let process = match HPROCESS::OpenProcess(co::PROCESS::QUERY_LIMITED_INFORMATION, false, pid) {
        Ok(process) => process,
        Err(error) if error == co::ERROR::INVALID_PARAMETER => return Ok(false),
        Err(error) => return Err(std::io::Error::other(error.to_string()).into()),
    };
    let code = process
        .GetExitCodeProcess()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(code == 259)
}
