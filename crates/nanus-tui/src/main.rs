//! The `nanus-tui` binary: the interactive interface, as a program of its own.
//!
//! ## Why it is separate from `nanus`
//!
//! The core binary hosts an agent and is deliberately small; this one draws a terminal
//! and is deliberately not. Keeping them apart is what lets the interface grow — more
//! panes, more keys, more rendering — without the thing a script runs growing with it,
//! and it is enforced by the manifests rather than by intention: this program links the
//! view, the link client, and the session store, and no agent whatsoever.
//!
//! ## What it talks to
//!
//! An agent, over the local link. By default that is the one a `nanus service` is
//! running; `--link` names any other, which is how `nanus tui` hands over the agent it
//! started for the shell. `--session` needs no agent at all, because a transcript that
//! has already been written down is just a file.
//!
//! `--resume` continues a conversation that already exists, by name or by id, and
//! `--name` starts one under a name. Either way the *agent* owns the session: the
//! interface attaches to it, reads its history from the store, and streams what happens
//! next.
//!
//! A bare `nanus-tui` is therefore the "connect to the long-running agent" mode, and the
//! whole of it: nothing about this program knows or cares how the agent was started.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// This crate is the program, so its contract *is* what it writes to stdout and stderr.
#![allow(clippy::print_stdout, clippy::print_stderr)]
// The binary's modules are internal to it, so `unreachable_pub` has nothing to say.
#![allow(unreachable_pub)]

use core::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use nanus_adapter_store::{JsonlStore, resolve_home};
use nanus_domain::ApprovalPolicy;
use nanus_link::paths::service_socket;
use nanus_ports::StoreHandle;
use nanus_tui::runtime::{Remote, Target, run_source, view};

/// The interactive nanus interface.
#[derive(Debug, Parser)]
#[command(
    name = "nanus-tui",
    version,
    about = "The interactive nanus interface",
    long_about = "The interactive nanus interface: a chat view over an agent, drawn in \
                  the terminal.\n\n\
                  Normally started by `nanus tui`, which runs an agent for the shell and \
                  hands it over with --link. Started on its own it connects to whatever \
                  `nanus service` is running, and with --session it reads a transcript \
                  instead of talking to an agent at all."
)]
struct Args {
    /// Talk to the agent listening at this socket.
    #[arg(long, value_name = "PATH", conflicts_with = "session")]
    link: Option<PathBuf>,

    /// Continue a session instead of starting one.
    ///
    /// The reference is a name or a session id, and the agent prefers a session it is
    /// already holding to one on disk — so resuming a conversation somebody is in the
    /// middle of joins it rather than opening a second copy.
    #[arg(long, value_name = "NAME|ID", conflicts_with = "session")]
    resume: Option<String>,

    /// Record a new session under this name.
    ///
    /// A name is how a session is found again, and it is taken for good: starting a
    /// second session with a name that is already held is refused.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["session", "resume"])]
    name: Option<String>,

    /// Read a recorded session instead of talking to an agent.
    ///
    /// With no id the most recent session is opened. Reading needs no agent and no API
    /// key, because the transcript is already written down.
    // Three states, not two: the flag absent, the flag given with no id, and the flag
    // given with an id. A single `Option` cannot say which of the last two it is, and
    // clap parses this shape directly. A sentinel id would be worse.
    #[allow(clippy::option_option)]
    #[arg(long, value_name = "ID", num_args = 0..=1)]
    session: Option<Option<String>>,

    /// Open the transcript this many rows back from the end.
    ///
    /// Only meaningful with `--session`: a live conversation has no history to open
    /// part way into.
    #[arg(long, value_name = "ROWS", default_value_t = 0)]
    scroll: u32,

    /// Show this approval state, and ask the agent to use it.
    ///
    /// `per_call`, `permitted`, or `all_calls`. `nanus tui` passes this through when the
    /// flag was given to it; Shift+Tab cycles the state afterwards.
    #[arg(long, value_name = "STATE")]
    approval: Option<ApprovalPolicy>,
}

fn main() -> ExitCode {
    // The kernel is single-threaded by design, so the runtime is current-thread.
    // Installing it here means every later `block_on` shares one reactor.
    nanus_kernel::runtime::install();
    let args = Args::parse();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The message goes to stderr so a redirected stdout stays clean.
            eprintln!("nanus-tui: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the mode the arguments asked for.
///
/// # Errors
///
/// Returns a rendered message for any failure: a store that will not open, a session
/// that does not exist, an agent that is not listening, or a terminal that is not there.
fn run(args: &Args) -> Result<(), String> {
    // One store for both modes: a recording is loaded from it, and a live conversation
    // reads its history out of it — the agent owns the session, but the store is where
    // what it has already said is written down.
    let store = block_on(open_store())?;
    // A live conversation has no history to open part way into, so `--scroll` without
    // `--session` is a request that cannot be honoured. Refused rather than ignored.
    if args.scroll > 0 && args.session.is_none() {
        return Err(String::from(
            "--scroll opens a recorded session part way back, so it needs --session",
        ));
    }
    if let Some(requested) = &args.session {
        return view(&store, requested.as_deref(), args.scroll, args.approval);
    }
    let socket = args.socket()?;
    let target = args.resume.as_ref().map_or_else(
        || Target::New {
            name: args.name.clone(),
        },
        |reference| Target::Resume(reference.clone()),
    );
    let mut remote =
        block_on(Remote::connect(&socket, &store, target, args.approval)).map_err(|error| {
            if args.link.is_some() {
                error
            } else {
                // Connecting to an agent nobody started is the common failure here, and the
                // useful answer is what to do about it rather than which syscall failed.
                format!(
                    "{error}\nstart one with `nanus service start`, or run `nanus tui` for an \
                 agent scoped to this shell"
                )
            }
        })?;
    run_source(&mut remote).map_err(|error| error.to_string())
}

impl Args {
    /// Returns the socket to connect to.
    ///
    /// `--link` names one; with no flag the service's socket is the only candidate, which
    /// is why a bare `nanus-tui` needs no mode of its own.
    ///
    /// # Errors
    ///
    /// Returns a message when the nanus home cannot be resolved.
    fn socket(&self) -> Result<PathBuf, String> {
        if let Some(path) = &self.link {
            return Ok(path.clone());
        }
        let home = resolve_home(None).map_err(|error| error.to_string())?;
        Ok(service_socket(&home))
    }
}

/// Opens the session store.
///
/// # Errors
///
/// Returns a message when the home cannot be resolved or the store cannot be opened.
async fn open_store() -> Result<StoreHandle, String> {
    let home = resolve_home(None).map_err(|error| error.to_string())?;
    JsonlStore::new(home)
        .await
        .map(JsonlStore::handle)
        .map_err(|error| error.to_string())
}

/// Drives a future to completion on the kernel runtime.
///
/// The setup half: connecting or opening a store awaits, and the interface half that
/// follows owns its own task set. Doing both in one `block_on` would nest them.
fn block_on<F: Future>(future: F) -> F::Output {
    nanus_kernel::runtime::block_on(future)
}
