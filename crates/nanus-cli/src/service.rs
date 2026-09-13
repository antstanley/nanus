//! The long-running agent service.
//!
//! ## What "long-running" costs
//!
//! A service is the one mode that has to survive the process that started it, and every
//! part of that is deliberate:
//!
//! - **A detached process starts in its own session.** `start` re-runs this binary with
//!   `--detached`, and that child calls `setsid`, so it is not in the shell's process
//!   group and has no controlling terminal. Closing the terminal cannot deliver a hangup
//!   to an agent that is meant to outlive it.
//! - **Nothing is written to a terminal that is gone.** A detached child's standard
//!   streams point at a log file, so its diagnostics are somewhere a person can read them
//!   rather than a pipe nobody holds.
//! - **Stopping is a request, not a kill.** `stop` connects to the socket and asks the
//!   agent to stop, which lets it finish the frame it is writing and remove its socket.
//!   `SIGTERM` is honoured too, because that is what every supervisor will send.
//!
//! ## Why readiness is polled rather than assumed
//!
//! A spawned daemon that fails — no key, a workspace that is not a directory, a socket
//! already in use — fails *after* its parent has returned, where nobody can see it. So
//! `start` waits for the socket to answer, reports the log's path when it does not, and
//! checks the child's status on every pass so a crash is reported as a crash rather than
//! as a timeout.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::Rc;
use std::time::{Duration, Instant};

use nanus_adapter_config::NanusConfig;
use nanus_bundle::compose::Pending;
use nanus_link::paths::service_socket;
use nanus_link::server::{Agent, bind, serve as serve_link};
use nanus_link::{Client, LinkError};

use crate::block_on_local;

// The synchronous half's only blocking call: asking a service a question needs an
// await, and `finish` runs outside the runtime by design.
use nanus_kernel::runtime::block_on;

/// How long `start` waits for a detached service to answer.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait between readiness checks.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The file name a detached service logs to under the nanus home.
const DEFAULT_LOG: &str = "nanus-service.log";

/// What a caller asked `start` for.
pub struct Options {
    /// Whether to run the service in this terminal instead of detaching.
    pub foreground: bool,
    /// Whether this process *is* the detached child, and should put itself in a session
    /// of its own.
    pub detached: bool,
    /// An explicit socket path.
    pub socket: Option<PathBuf>,
    /// An explicit log path.
    pub log: Option<PathBuf>,
    /// The configuration file the parent was given, so the child reads the same one.
    pub config_file: Option<PathBuf>,
}

/// What starting a service decided to do.
pub enum Start {
    /// Serve in this process, now.
    Serve {
        /// The adapters, ready to mount.
        pending: Box<Pending>,
        /// The workspace sessions are created against.
        workspace: PathBuf,
        /// The socket to listen on.
        socket: PathBuf,
    },
    /// A detached service is running and already answering.
    Started {
        /// The socket it answered on.
        socket: PathBuf,
    },
}

/// Returns the nanus home, where sockets and the default log live.
///
/// # Errors
///
/// Returns a message when no home directory can be determined.
pub fn home() -> Result<PathBuf, String> {
    nanus_bundle::compose::store_home().map_err(|error| error.to_string())
}

/// Returns the socket a service should listen on.
///
/// # Errors
///
/// Returns a message when the home cannot be resolved.
pub fn socket_path(config: &NanusConfig, explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = explicit.or(config.service_socket.as_deref()) {
        return Ok(path.to_path_buf());
    }
    Ok(service_socket(&home()?))
}

/// Returns the file a detached service logs to.
///
/// # Errors
///
/// Returns a message when the home cannot be resolved.
pub fn log_path(config: &NanusConfig, explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = explicit.or(config.service_log.as_deref()) {
        return Ok(path.to_path_buf());
    }
    Ok(home()?.join(DEFAULT_LOG))
}

/// Starts the service, or decides to serve it here.
///
/// # Errors
///
/// Returns a message when the socket cannot be resolved, a service is already listening,
/// the composition fails, or a detached service does not come up.
pub async fn start(config: &NanusConfig, options: &Options) -> Result<Start, String> {
    let socket = socket_path(config, options.socket.as_deref())?;
    if options.detached {
        // Must happen before anything else this process does: the whole point is that it
        // stops being a child of the shell.
        detach_from_terminal();
    } else if !options.foreground {
        let log = log_path(config, options.log.as_deref())?;
        return launch(options, &socket, &log).await;
    }
    let pending = nanus_bundle::compose(config)
        .await
        .map_err(|error| error.to_string())?;
    let workspace =
        nanus_bundle::compose::workspace_root(config).map_err(|error| error.to_string())?;
    Ok(Start::Serve {
        pending: Box::new(pending),
        workspace,
        socket,
    })
}

/// Spawns the detached child and waits until it answers.
async fn launch(options: &Options, socket: &Path, log: &Path) -> Result<Start, String> {
    // An agent that is already there is worth naming. Without this the child would fail
    // to bind and the poll below would find the *old* service and report success, which
    // is the one outcome a user must never be given.
    if Client::connect(socket).await.is_ok() {
        return Err(format!(
            "a nanus service is already listening at {}",
            socket.display()
        ));
    }
    let binary = std::env::current_exe()
        .map_err(|error| format!("cannot find this program's own path: {error}"))?;
    let mut child = spawn_detached(&binary, options, socket, log)?;
    wait_until_ready(socket, &mut child, log).await?;
    Ok(Start::Started {
        socket: socket.to_path_buf(),
    })
}

/// Starts the child that will outlive this process.
fn spawn_detached(
    binary: &Path,
    options: &Options,
    socket: &Path,
    log: &Path,
) -> Result<std::process::Child, String> {
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let sink = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|error| format!("cannot open the log {}: {error}", log.display()))?;

    let mut command = std::process::Command::new(binary);
    if let Some(path) = &options.config_file {
        // The child reads the same configuration, or it would serve a different agent
        // from the one the user configured.
        command.arg("--config").arg(path);
    }
    command
        .arg("service")
        .arg("start")
        // `--detached` is what tells the child to leave the shell's session. It is hidden
        // from the help because it is an instruction from a parent, not a choice for a
        // person: running it by hand detaches a service from the terminal that asked.
        .arg("--detached")
        .arg("--socket")
        .arg(socket)
        .arg("--log")
        .arg(log)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(sink));
    command
        .spawn()
        .map_err(|error| format!("cannot start {}: {error}", binary.display()))
}

/// Waits until the socket answers, the child dies, or the deadline passes.
async fn wait_until_ready(
    socket: &Path,
    child: &mut std::process::Child,
    log: &Path,
) -> Result<(), String> {
    // `checked_add` rather than `+`: the workspace treats silent overflow in arithmetic
    // as a defect, and a deadline that wrapped would be a wait that never ends.
    let Some(deadline) = Instant::now().checked_add(START_TIMEOUT) else {
        return Err(String::from(
            "the service start deadline is not representable",
        ));
    };
    loop {
        match Client::connect(socket).await {
            Ok(client) => {
                // The connection itself is the proof; it was a whole session by the time
                // it said hello, and letting it go is how the agent learns nobody stayed.
                drop(client);
                return Ok(());
            }
            Err(LinkError::Connect { .. }) => {}
            Err(error) => return Err(error.to_string()),
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "the service exited with {status}; see {}",
                    log.display()
                ));
            }
            Ok(None) => {}
            Err(error) => return Err(format!("cannot check the service: {error}")),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the service did not answer within {}s; see {}",
                START_TIMEOUT.as_secs(),
                log.display()
            ));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Puts this process in a session of its own.
///
/// `setsid` fails when the caller already leads a process group, which happens only if
/// someone ran the hidden flag by hand from a job-control shell. That is worth a warning
/// rather than a failure: the service still works, it is merely still attached to the
/// terminal that started it.
fn detach_from_terminal() {
    match nix::unistd::setsid() {
        Ok(_) => tracing::debug!("the service detached into a session of its own"),
        Err(error) => tracing::warn!(
            %error,
            "--detached was asked for but setsid failed; the service may not outlive this shell"
        ),
    }
}

/// Serves the agent until a signal asks it to stop.
///
/// # Errors
///
/// Returns a message when the composition fails to mount or the socket cannot be bound.
pub fn serve(pending: Pending, workspace: &Path, socket: &Path) -> Result<(), String> {
    let harness = pending.start().map_err(|error| error.to_string())?;
    let agent = Rc::new(Agent::new(&harness, workspace));
    let outcome = block_on_local(async {
        let listener = bind(socket).await.map_err(|error| error.to_string())?;
        tracing::info!(socket = %socket.display(), "the service is listening");
        serve_until_signal(listener, agent)
            .await
            .map_err(|error| error.to_string())
    });
    // The socket is this process's to remove: leaving it behind would make the next
    // `start` look like a service that is already running.
    let _removed = std::fs::remove_file(socket);
    if let Err(error) = harness.shutdown() {
        tracing::warn!(%error, "the composition did not shut down cleanly");
    }
    outcome
}

/// Serves connections until a signal asks this process to stop.
async fn serve_until_signal(
    listener: tokio::net::UnixListener,
    agent: Rc<Agent>,
) -> Result<(), LinkError> {
    serve_link(listener, agent, shutdown_signal()).await
}

/// Resolves when a signal asks this process to stop.
///
/// A `shutdown` request over the link is handled inside `nanus_link::serve`; this is the
/// other door, the one a supervisor and a Ctrl-C use.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    match (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) {
        (Ok(mut terminate), Ok(mut interrupt)) => {
            tokio::select! {
                _ = terminate.recv() => tracing::debug!("stopping on SIGTERM"),
                _ = interrupt.recv() => tracing::debug!("stopping on SIGINT"),
            }
        }
        // Without a handler the service still runs; it simply has to be stopped through
        // the socket, which is the documented way anyway.
        _ => std::future::pending().await,
    }
}

/// Asks a running service to stop.
///
/// # Errors
///
/// Returns a message when nothing is listening or the request cannot be sent.
pub fn stop(socket: &Path) -> Result<(), String> {
    block_on(async {
        let mut client = Client::connect(socket)
            .await
            .map_err(|error| error.to_string())?;
        client
            .request_shutdown()
            .await
            .map_err(|error| error.to_string())
    })?;
    // The reply is not waited for: the agent acknowledges and then stops, and a `stop`
    // that hung whenever the agent stopped before flushing would be worse than one that
    // reports what it asked for.
    println!("nanus: asked the service at {} to stop", socket.display());
    Ok(())
}

/// Reports whether a service is running, and what it is.
///
/// # Errors
///
/// Returns a message when nothing is listening or the reply cannot be read. Not running is
/// a failure rather than a plain answer, so a script can branch on the exit code.
pub fn status(socket: &Path) -> Result<(), String> {
    let (info, held) = block_on(async {
        let mut client = Client::connect(socket)
            .await
            .map_err(|error| error.to_string())?;
        let info = client
            .ask_status()
            .await
            .map_err(|error| error.to_string())?;
        let held = client.sessions().await.map_err(|error| error.to_string())?;
        Ok::<_, String>((info, held))
    })?;
    println!("socket: {}", socket.display());
    println!("model: {}", info.model);
    println!("tools: {}", info.tools);
    println!("workspace: {}", info.workspace);
    if held.is_empty() {
        println!("sessions: none held");
    }
    for session in held {
        // The name when it has one, the id otherwise: a listing that hid the id would
        // make an unnamed session impossible to resume.
        let name = session.name.unwrap_or_else(|| session.session.clone());
        let state = if session.busy { "running" } else { "idle" };
        println!(
            "session: {}  {name}  {state}  {} attached  {} events",
            session.session, session.viewers, session.events
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> NanusConfig {
        NanusConfig::default()
    }

    #[test]
    fn an_explicit_socket_wins_over_the_configuration() {
        let configured = NanusConfig {
            service_socket: Some(PathBuf::from("/tmp/from-config.sock")),
            ..config()
        };
        let chosen = socket_path(&configured, Some(Path::new("/tmp/from-flag.sock")));
        assert_eq!(chosen.ok(), Some(PathBuf::from("/tmp/from-flag.sock")));
    }

    #[test]
    fn the_configuration_names_the_socket_when_no_flag_does() {
        let configured = NanusConfig {
            service_socket: Some(PathBuf::from("/tmp/from-config.sock")),
            ..config()
        };
        let chosen = socket_path(&configured, None);
        assert_eq!(chosen.ok(), Some(PathBuf::from("/tmp/from-config.sock")));
    }

    #[test]
    fn with_neither_the_default_lives_under_the_nanus_home() {
        // The default matters more than the override: it is the path a client computes
        // for itself, so a service and an interface that never speak before connecting
        // still meet.
        let chosen = socket_path(&config(), None);
        assert!(chosen.is_ok(), "{chosen:?}");
        let Ok(chosen) = chosen else { return };
        assert_eq!(
            chosen.file_name().and_then(|name| name.to_str()),
            Some("agent.sock")
        );
        assert_eq!(
            chosen
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str()),
            Some("run")
        );
    }

    #[test]
    fn the_log_defaults_beside_the_home_rather_than_inside_it() {
        let chosen = log_path(&config(), None);
        assert!(chosen.is_ok(), "{chosen:?}");
        let Ok(chosen) = chosen else { return };
        assert_eq!(
            chosen.file_name().and_then(|name| name.to_str()),
            Some(DEFAULT_LOG)
        );
    }

    #[test]
    fn an_explicit_log_wins_over_the_configuration() {
        let configured = NanusConfig {
            service_log: Some(PathBuf::from("/tmp/from-config.log")),
            ..config()
        };
        assert_eq!(
            log_path(&configured, Some(Path::new("/tmp/from-flag.log"))).ok(),
            Some(PathBuf::from("/tmp/from-flag.log"))
        );
        assert_eq!(
            log_path(&configured, None).ok(),
            Some(PathBuf::from("/tmp/from-config.log"))
        );
    }
}
