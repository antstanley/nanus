//! Starting the interface, and the shell-scoped agent it talks to.
//!
//! ## The shape of `nanus tui`
//!
//! The core binary does not draw. It composes an agent, binds a socket only this user can
//! reach, runs the interface **as a child process** with the terminal inherited, and
//! serves the agent until that child exits. The interface is then a client like any
//! other; what makes this one special is only that its agent's lifetime is its own.
//!
//! That ordering is the whole design, and each part of it is load-bearing:
//!
//! - The socket is bound *before* the child starts, so the child's first connect cannot
//!   race the listener.
//! - The child inherits the terminal rather than taking a copy of it, because raw mode
//!   and the alternate screen are process-wide state.
//! - The agent stops when the child exits, which is what "scoped to the shell session"
//!   means in practice: closing the interface closes the agent, and there is no orphan
//!   holding a tool process open.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use nanus_adapter_config::CONFIG_ENV;
use nanus_bundle::compose::Pending;
use nanus_link::paths::attached_endpoint;
use nanus_link::server::{Agent, bind, serve};

use crate::block_on_local;

/// The environment variable that overrides which interface binary is run.
///
/// Documented because it is the escape hatch for a build layout this binary cannot
/// predict — a `cargo install` of one package, a wrapper script, a test.
pub const TUI_BIN_ENV: &str = "NANUS_TUI";

/// The name of the interface program that ships beside this one.
const TUI_BIN: &str = "nanus-tui";

/// Returns the interface binary that belongs with the program at `exe`.
///
/// Beside it, rather than found on `PATH`: the two are built and installed together, and
/// a `PATH` lookup would happily run a different version's interface against this core's
/// protocol.
#[must_use]
pub fn interface_binary(exe: &Path) -> PathBuf {
    let parent = exe.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{TUI_BIN}{}", std::env::consts::EXE_SUFFIX))
}

/// Returns the interface binary to run, reporting where it looked.
///
/// # Errors
///
/// Returns a message when the binary does not exist, naming the path that was tried and
/// the environment variable that would override it.
pub fn resolve_binary() -> Result<PathBuf, String> {
    let path = if let Some(raw) = std::env::var_os(TUI_BIN_ENV) {
        PathBuf::from(raw)
    } else {
        let exe = std::env::current_exe()
            .map_err(|error| format!("cannot find this program's own path: {error}"))?;
        interface_binary(&exe)
    };
    if !path.is_file() {
        return Err(format!(
            "the interface is not built: {} does not exist.\n\
             build it with `cargo build --release`, or name it with {TUI_BIN_ENV}",
            path.display()
        ));
    }
    Ok(path)
}

/// Runs the interface with an agent this process owns.
///
/// `home` is the nanus home the agent was composed in, where its socket is bound, and
/// `config_file` the configuration this command was given, which the interface reads too: it
/// opens the session store itself to show a conversation's history, so it has to open the
/// store the agent records into.
///
/// # Errors
///
/// Returns a message when the composition fails to mount, the socket cannot be bound, the
/// interface cannot be started, or it exits non-zero.
pub fn attached(
    pending: Pending,
    workspace: &Path,
    home: &Path,
    config_file: Option<&Path>,
    arguments: &[OsString],
) -> Result<(), String> {
    let binary = resolve_binary()?;
    let socket = attached_endpoint(home, std::process::id()).map_err(|error| error.to_string())?;
    let harness = pending.start().map_err(|error| error.to_string())?;
    let agent = Rc::new(Agent::new(&harness, workspace));

    // The agent is torn down and the socket removed whatever the loop does, so a failure
    // part way through cannot leave either behind.
    let outcome = block_on_local(async {
        // The failures are carried out rather than propagated, because the block also has
        // to produce the child's exit status: a `?` here would leave the socket behind.
        let listener = match bind(&socket).await {
            Ok(listener) => listener,
            Err(error) => return (Err(error.to_string()), None),
        };
        let mut child = match spawn(&binary, &socket, config_file, arguments) {
            Ok(child) => child,
            Err(error) => return (Err(error), None),
        };
        let (exited, status) = tokio::sync::oneshot::channel();
        let served = serve(listener, agent, async move {
            // The agent's lifetime is the child's: the moment the interface is gone,
            // there is nobody left to serve.
            let _ = exited.send(child.wait().await.ok());
        })
        .await
        .map_err(|error| error.to_string());
        (served, status.await.ok().flatten())
    });

    #[cfg(unix)]
    let _removed = std::fs::remove_file(&socket);
    if let Err(error) = harness.shutdown() {
        tracing::warn!(%error, "the composition did not shut down cleanly");
    }
    let (served, status) = outcome;
    served?;
    report_exit(status)
}

/// Runs the interface with no agent at all.
///
/// Used for a recorded session, which needs no model: a transcript that has already been
/// written down is just a file, and demanding an agent to read one would be a barrier
/// with no purpose. The configuration file goes to it as it does in [`spawn`], so the
/// transcript is read from the home that configuration names.
///
/// # Errors
///
/// Returns a message when the interface cannot be started or exits non-zero.
pub fn alone(arguments: &[OsString], config_file: Option<&Path>) -> Result<(), String> {
    let binary = resolve_binary()?;
    let mut command = std::process::Command::new(&binary);
    hand_on_configuration(&mut command, config_file);
    let status = command
        .args(arguments)
        // Inherited rather than piped: this child is the interface, and it owns the
        // terminal for as long as it runs.
        .status()
        .map_err(|error| format!("cannot start {}: {error}", binary.display()))?;
    report_exit(Some(status))
}

/// Starts the interface against the agent listening at `socket`.
///
/// `arguments` is what the interface was told about which conversation to open; the
/// socket is this function's business and the choice is not, so the two are passed
/// together rather than merged above.
///
/// The configuration file, when this command was given one with `--config`, goes to the
/// interface as `NANUS_CONFIG`, the variable it already reads. Its display preferences and its
/// nanus home are both in that file, and an interface that read the default file instead would
/// draw with the wrong preferences and read history from a home the agent is not recording into.
fn spawn(
    binary: &Path,
    socket: &Path,
    config_file: Option<&Path>,
    arguments: &[OsString],
) -> Result<tokio::process::Child, String> {
    let mut command = tokio::process::Command::new(binary);
    hand_on_configuration(command.as_std_mut(), config_file);
    // Killed if this process gives up on it: an interface talking to an agent that no
    // longer exists would connect to nothing and look broken. `kill_on_drop` covers the
    // paths that exit early, and the ordinary path has already reaped the child.
    command
        .arg("--link")
        .arg(socket)
        .args(arguments)
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("cannot start {}: {error}", binary.display()))
}

/// Gives the interface the configuration file this command was given, as `NANUS_CONFIG`.
///
/// Only when there was one: with no `--config`, the interface resolves the file exactly as
/// this process did — an inherited `NANUS_CONFIG`, else the platform location — so the
/// variable is left as it was rather than set to a path this process never named.
fn hand_on_configuration(command: &mut std::process::Command, config_file: Option<&Path>) {
    if let Some(file) = config_file {
        command.env(CONFIG_ENV, file);
    }
}

/// Turns a child's exit into this process's outcome.
///
/// A non-zero status is reported rather than swallowed: the interface failing to start —
/// because there is no terminal, or because the agent went away — is this command
/// failing, and a caller that only ever checked this process's exit code should see that.
fn report_exit(status: Option<std::process::ExitStatus>) -> Result<(), String> {
    match status {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(format!("the interface exited with {status}")),
        // No status means the wait itself failed, which the interface has already
        // reported on its own stderr.
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    /// A `--config` reaches the interface as the variable it reads, and its absence leaves
    /// the variable to be inherited rather than set or cleared.
    #[test]
    fn the_interface_is_given_the_configuration_this_command_was_given() {
        let config_env = |command: &std::process::Command| {
            command
                .get_envs()
                .find(|(name, _)| *name == std::ffi::OsStr::new(CONFIG_ENV))
                .map(|(_, value)| value.map(std::ffi::OsStr::to_os_string))
        };
        let file = Path::new("/srv/example-host/config.toml");
        let mut given = std::process::Command::new("nanus-tui");
        hand_on_configuration(&mut given, Some(file));
        assert_eq!(
            config_env(&given),
            Some(Some(file.as_os_str().to_os_string()))
        );

        let mut none = std::process::Command::new("nanus-tui");
        hand_on_configuration(&mut none, None);
        assert_eq!(config_env(&none), None, "nothing set and nothing removed");
    }

    use super::*;

    #[test]
    fn the_interface_is_looked_for_beside_the_core() {
        let exe = Path::new("/opt/nanus/bin/nanus");
        let found = interface_binary(exe);
        assert_eq!(
            found,
            Path::new("/opt/nanus/bin").join(format!("nanus-tui{}", std::env::consts::EXE_SUFFIX))
        );
        // Beside, not on `PATH`: two versions of the protocol must not meet.
        assert_eq!(found.parent(), exe.parent());
    }

    #[test]
    fn a_core_with_no_directory_still_resolves_to_a_name() {
        // A bare program name has no parent, which is not a reason to panic on the way to
        // reporting that the interface is missing.
        let found = interface_binary(Path::new("nanus"));
        assert!(found.to_string_lossy().contains(TUI_BIN), "{found:?}");
    }

    #[test]
    fn a_successful_child_is_not_an_error_and_a_failed_one_is() {
        // The two directions of the exit contract, without spawning anything: a status
        // that cannot be obtained and one that is missing are different from a failure.
        assert!(report_exit(None).is_ok());
        let failure = std::process::Command::new("false").status();
        assert!(failure.is_ok(), "the fixture runs");
        let Ok(failure) = failure else { return };
        let reported = report_exit(Some(failure));
        assert!(reported.is_err(), "{reported:?}");
    }
}
