//! A detached launcher excludes its original standard handles from inheritance.

use std::os::windows::io::AsRawHandle as _;
use std::os::windows::process::CommandExt as _;
use std::process::{Child, Command};

use bun_windows_sys::{INVALID_HANDLE_VALUE, kernel32::SetHandleInformation};

/// A failure to isolate a detached child or to spawn it.
#[derive(Debug, thiserror::Error)]
#[error("the Windows detached process failed: {0}")]
pub struct DetachError(#[from] std::io::Error);

/// Spawns a detached process with the command's explicitly configured standard streams.
///
/// The launcher's original standard handles become non-inheritable for subsequent spawns.
/// The caller must configure the child's streams: Rust duplicates those into new inheritable
/// handles during spawning. The original handles remain open and usable by this process.
///
/// # Errors
///
/// Returns an error if inheritance cannot be cleared on a present standard handle or spawning
/// fails. Missing standard handles are allowed, so a launcher needs no terminal.
///
/// The child asks to break away from any Job Object the launcher is in, so a service started
/// from a session whose job is killed on close (an OpenSSH session, a CI step) survives it. A
/// job that does not permit breaking away refuses the request with "access denied"; the spawn is
/// then retried inside the job, and the service lives exactly as long as that job does — which is
/// what a job that forbids leaving it is asking for. Being in no job at all is not an error.
pub fn spawn_detached(command: &mut Command) -> Result<Child, DetachError> {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const ERROR_ACCESS_DENIED: i32 = 5;
    isolate_standard_handles()?;
    let detached = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    let child = match command
        .creation_flags(detached | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
    {
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
            command.creation_flags(detached).spawn()?
        }
        spawned => spawned?,
    };
    assert!(child.id() > 0, "a detached child has a valid process id");
    Ok(child)
}

fn isolate_standard_handles() -> std::io::Result<()> {
    const HANDLE_FLAG_INHERIT: u32 = 1;
    for handle in [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ] {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            continue;
        }
        // These are borrowed handles, never owned or closed here. The binding marks this
        // call safe: the kernel validates the opaque handle; the other arguments are values.
        // Clearing only INHERIT preserves every other handle flag and the stream's data.
        if SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use super::*;

    #[test]
    fn explicit_child_output_survives_detaching() {
        let mut command = Command::new("cmd");
        command
            .args(["/C", "echo detached-output"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = spawn_detached(&mut command)
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            "detached-output"
        );
    }

    #[test]
    fn a_missing_detached_program_is_an_error() {
        let mut command = Command::new("nanus-no-such-detached-program-72432.exe");
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        assert!(spawn_detached(&mut command).is_err());
    }
}
