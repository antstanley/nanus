//! Argument parsing and the modes.
//!
//! ## The three modes
//!
//! | Mode | What it is |
//! |---|---|
//! | `nanus run <task>` | One prompt, one answer on stdout, exit. |
//! | `nanus tui` (or a bare `nanus`) | An agent scoped to this shell, and the interface to talk to it. |
//! | `nanus service` | An agent that outlives the shell that started it. |
//!
//! ## Sessions
//!
//! A session is the conversation, and it is written down whether it was started by a run,
//! an interface, or a service. `--name` records one under a name, `--resume` continues
//! one, and `nanus sessions` lists them. The name is an alias for the store key, so a
//! session keeps its identity through a rename, and a name is refused rather than moved
//! when another session already answers to it.
//!
//! ## What this binary is not
//!
//! It is not the interface. It hosts an agent, and for the interactive mode it runs the
//! interface as a separate program and serves that program over a local socket. Two
//! decades of terminal interfaces have taught the same lesson — an interface grows — and
//! the core that a script invokes is the last thing that should grow with it. So the
//! split is enforced by the manifest: `nanus-cli` does not depend on `nanus-tui` at all,
//! and could not call into it if it wanted to.
//!
//! ## What the modes share
//!
//! Everything that matters: one configuration loader, one composition, one answer to which
//! workspace a session belongs to. `run`, `tui`, and `service` differ in how long the
//! agent lives, not in what the agent is.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser, Subcommand};
use nanus_adapter_config::NanusConfig;
use nanus_bundle::compose::open_store;
use nanus_bundle::{Harness, compose};
use nanus_kernel::runtime::block_on as kernel_block_on;
use std::ffi::OsString;

use crate::progress::StderrProgress;

/// A coding agent harness.
#[derive(Debug, Parser)]
#[command(
    name = "nanus",
    version,
    about = "A coding agent harness",
    long_about = "A coding agent harness built on a Rust implementation of the Cordis \
                  meta-framework. Run one task and print its answer, sit in front of the \
                  interactive interface, or run an agent as a service.\n\n\
                  With no subcommand and a terminal, nanus starts the interactive \
                  interface; with no terminal it prints this help instead."
)]
pub struct Args {
    /// Report reasoning and tool activity on stderr as it happens.
    #[arg(long, global = true)]
    pub verbose: bool,

    /// Print nothing but the answer on stdout.
    #[arg(long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Load configuration from this file instead of the default location.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// What the binary was asked to do.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run one task, print its answer, and exit.
    Run {
        /// Record the session under this name.
        ///
        /// A name is how a session is found again, so it is taken for good: starting a
        /// second session with a name that is already held is an error rather than a
        /// silent second meaning for the same word.
        #[arg(long, value_name = "NAME", conflicts_with = "resume")]
        name: Option<String>,

        /// Continue a session instead of starting one.
        ///
        /// The reference is a name or a session id. This is a second writer on the
        /// conversation: a session a `nanus service` is holding open is not locked, so
        /// resuming one that is live elsewhere is a way to lose a turn.
        #[arg(long, value_name = "NAME|ID")]
        resume: Option<String>,

        /// The task, as one or more words.
        #[arg(required = true, value_name = "TASK")]
        task: Vec<String>,
    },

    /// Start the interactive interface.
    ///
    /// The same thing a bare `nanus` does when it has a terminal. The interface is a
    /// separate program; this one runs an agent for the shell and hands it over.
    #[command(visible_alias = "ui")]
    Tui {
        /// Read a recorded session instead of talking to an agent.
        ///
        /// With no id the most recent session is opened. Reading needs no API key,
        /// because the transcript is already written down.
        // Three states, not two: the flag absent, the flag given with no id, and the
        // flag given with an id. A single `Option` cannot say which of the last two it
        // is, and clap parses this shape directly. A sentinel id would be worse.
        #[allow(clippy::option_option)]
        #[arg(long, value_name = "ID", num_args = 0..=1)]
        session: Option<Option<String>>,

        /// Open the transcript this many rows back from the end.
        ///
        /// Only meaningful with `--session`: a live conversation has no history to open
        /// part way into.
        #[arg(long, value_name = "ROWS", default_value_t = 0)]
        scroll: u32,

        /// Talk to the agent a `nanus service` is running instead of starting one.
        #[arg(long, conflicts_with = "session")]
        connect: bool,

        /// Connect to a service listening on this socket instead of the configured one.
        ///
        /// Only meaningful with `--connect`, and the way to reach a second service that
        /// was started with its own `--socket`.
        #[arg(long, value_name = "PATH", requires = "connect")]
        socket: Option<PathBuf>,

        /// Continue a session instead of starting one.
        ///
        /// The reference is a name or a session id. The agent prefers a session it is
        /// already holding to one on disk, so resuming a conversation somebody is in the
        /// middle of joins it.
        #[arg(long, value_name = "NAME|ID", conflicts_with = "session")]
        resume: Option<String>,

        /// Record a new session under this name.
        #[arg(long, value_name = "NAME", conflicts_with_all = ["session", "resume"])]
        name: Option<String>,
    },

    /// Run the agent as a long-running service.
    ///
    /// The agent outlives the shell that started it: it detaches into a session of its
    /// own and answers on a socket only this user can reach.
    Service {
        /// What to do with the service.
        #[command(subcommand)]
        action: ServiceAction,
    },

    /// Show the effective configuration.
    Config,

    /// List recorded sessions, newest first.
    Sessions {
        /// What to do with them other than listing.
        #[command(subcommand)]
        action: Option<SessionsAction>,
    },
}

/// What to do with recorded sessions.
#[derive(Debug, Subcommand)]
pub enum SessionsAction {
    /// Record a name for an existing session.
    ///
    /// Naming a session that already has one renames it, and the old name is released:
    /// one session has one name, and a name has one session.
    Name {
        /// The name to record.
        name: String,
        /// The session to record it for: an id, or a name it already answers to.
        session: String,
    },
}

/// What to do with the service.
#[derive(Debug, Subcommand)]
pub enum ServiceAction {
    /// Start the service.
    ///
    /// Detached by default, so it survives the shell. `--foreground` runs it here, which
    /// is what a supervisor such as systemd wants.
    Start {
        /// Run in this terminal instead of detaching.
        #[arg(long)]
        foreground: bool,

        /// Internal: marks the child that `start` spawned, which detaches itself.
        ///
        /// Hidden because it is an instruction from a parent rather than a choice for a
        /// person: running it by hand detaches a service from the terminal that asked.
        #[arg(long, hide = true)]
        detached: bool,

        /// Listen on this socket instead of the configured one.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,

        /// Write the detached service's output here instead of the default log.
        #[arg(long, value_name = "PATH")]
        log: Option<PathBuf>,
    },

    /// Ask a running service to stop.
    Stop {
        /// The socket the service is listening on.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
    },

    /// Report whether a service is running, and what it is.
    Status {
        /// The socket the service is listening on.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
    },
}

/// Renders the usage text.
///
/// Built from the same [`Args`] the parser uses, so the help a person reads and the
/// grammar they are held to cannot disagree.
pub fn help_text() -> String {
    Args::command().render_help().to_string()
}

/// Whether there is a terminal to hand to the interface.
///
/// Asked here, before anything is composed or started, so the answer does not depend on
/// whether an API key happens to be configured — and so a piped `nanus tui` fails with a
/// sentence about a terminal rather than by starting an agent for nobody.
#[must_use]
pub fn interactive() -> bool {
    std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
}

/// Parses the arguments, prepares what needs awaiting, and then finishes the work.
///
/// The two halves exist because of one hard constraint: **the kernel drives its plugin
/// hooks with `block_on`, and `block_on` panics when called from inside a runtime.**
/// So everything that has to be awaited — opening a session store, streaming a model —
/// happens here, and everything that has to *block* — mounting the composition, reading
/// the transcript, running a turn — happens in [`finish`], after this future has
/// resolved and the caller has left the runtime.
///
/// Getting this split wrong is not a style question: it is the difference between a
/// working command and "cannot start a runtime from within a runtime".
///
/// # Errors
///
/// Returns a rendered message for any failure. The caller turns it into an exit code:
/// the message is already user-facing, so it is not wrapped again.
pub async fn prepare() -> Result<Ready, String> {
    // `try_parse` rather than `parse`, so a usage error is reported by the same
    // path as every other failure instead of exiting from inside the library.
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(error) => {
            // A help or version request is a successful outcome, not a failure, so it
            // is printed and treated as done.
            let rendered = error.render().to_string();
            print!("{rendered}");
            return if error.use_stderr() {
                Err(String::from("invalid arguments"))
            } else {
                Ok(Ready::Done)
            };
        }
    };
    // The subcommand is taken by value so the task words move rather than clone, and
    // the global options are read from a separate borrow taken first.
    let Args {
        verbose,
        quiet: _,
        config,
        command,
    } = args;
    let options = Options {
        verbose,
        config: config.clone(),
    };
    let command = match command {
        Some(command) => command,
        // A bare `nanus` is the interface: someone who types the program's name and
        // nothing else wants the program, not a summary of its grammar.
        None if interactive() => Command::Tui {
            session: None,
            scroll: 0,
            connect: false,
            socket: None,
            resume: None,
            name: None,
        },
        // Redirected or piped there is no screen to draw on, and starting a
        // full-screen interface there would either fail or hang. Saying what the
        // program can do is the useful answer, and it is not a failure: nothing was
        // asked for and the question was answered.
        None => {
            print!("{}", help_text());
            return Ok(Ready::Done);
        }
    };
    // The interface needs a terminal, and saying so here rather than letting the drawing
    // library discover it is the difference between a clear message and an abort: taking
    // a screen that is not there panics. Checked before anything is composed or opened,
    // so the answer does not depend on whether an API key happens to be configured.
    if matches!(command, Command::Tui { .. }) && !interactive() {
        return Err(String::from(
            "the interactive interface needs a terminal; \
             `nanus run <task>` and `nanus sessions` work without one",
        ));
    }
    match command {
        Command::Run { task, resume, name } => prepare_run(&options, &task, resume, name).await,
        // Showing the configuration prints and is finished.
        Command::Config => show_config(&options).map(|()| Ready::Done),
        Command::Sessions { action } => prepare_sessions(action).await,
        Command::Tui {
            session,
            scroll,
            connect,
            socket,
            resume,
            name,
        } => prepare_tui(&options, session, scroll, connect, socket, resume, name).await,
        Command::Service { action } => prepare_service(&options, action).await,
    }
}

/// What has been awaited, and what still has to be done synchronously.
// The variants carry erased port handles and other non-`Debug` values; the enum exists
// to move work between two phases, not to be printed.
pub enum Ready {
    /// Nothing left to do; the output is already written.
    Done,
    /// A composition is built but not mounted.
    Run {
        /// The adapters, ready to mount.
        pending: Box<compose::Pending>,
        /// The workspace a session is created against.
        workspace: PathBuf,
        /// The task to run.
        prompt: String,
        /// Whether to report progress on stderr.
        verbose: bool,
        /// The session to continue, when one was named.
        resume: Option<String>,
        /// The name to record a new session under.
        name: Option<String>,
    },
    /// The session store is open and its contents are ready to read.
    List {
        /// The store to read from.
        store: nanus_ports::StoreHandle,
    },
    /// A name is ready to be recorded against a session.
    Name {
        /// The store that holds both.
        store: nanus_ports::StoreHandle,
        /// The name to record.
        name: String,
        /// The session to record it for.
        session: String,
    },
    /// A composition is built for a shell-scoped agent the interface will talk to.
    Tui {
        /// The adapters, ready to mount.
        pending: Box<compose::Pending>,
        /// The workspace a session is created against.
        workspace: PathBuf,
        /// What to tell the interface beyond which socket to use.
        arguments: Vec<OsString>,
    },
    /// The interface needs no agent: it was asked to read a transcript.
    Spawn {
        /// The arguments to hand the interface program.
        arguments: Vec<OsString>,
    },
    /// Serve an agent until a signal or a client asks it to stop.
    Serve {
        /// The adapters, ready to mount.
        pending: Box<compose::Pending>,
        /// The workspace a session is created against.
        workspace: PathBuf,
        /// The socket to listen on.
        socket: PathBuf,
    },
    /// Ask a running service to stop.
    Stop {
        /// The socket that service is listening on.
        socket: PathBuf,
    },
    /// Report whether a service is running.
    Status {
        /// The socket that service would be listening on.
        socket: PathBuf,
    },
}

/// Completes the work [`prepare`] set up, synchronously.
///
/// Deliberately **not** `async`: every step here either mounts a kernel, drives the agent
/// loop with `block_on`, or runs a child that owns the terminal. All of those are
/// illegal or pointless inside a runtime.
///
/// # Errors
///
/// Returns a rendered message for any failure.
pub fn finish(ready: Ready) -> Result<(), String> {
    match ready {
        Ready::Done => Ok(()),
        Ready::Run {
            pending,
            workspace,
            prompt,
            verbose,
            resume,
            name,
        } => run_turn(
            *pending,
            &workspace,
            &prompt,
            verbose,
            resume.as_deref(),
            name.as_deref(),
        ),
        Ready::List { store } => print_sessions(&store),
        Ready::Name {
            store,
            name,
            session,
        } => record_name(&store, &name, &session),
        Ready::Tui {
            pending,
            workspace,
            arguments,
        } => crate::tui::attached(*pending, &workspace, &arguments),
        Ready::Spawn { arguments } => crate::tui::alone(&arguments),
        Ready::Serve {
            pending,
            workspace,
            socket,
        } => crate::service::serve(*pending, &workspace, &socket),
        Ready::Stop { socket } => crate::service::stop(&socket),
        Ready::Status { socket } => crate::service::status(&socket),
    }
}

/// The global options, separated from the subcommand.
struct Options {
    verbose: bool,
    config: Option<PathBuf>,
}

/// Loads the configuration, reporting a malformed file rather than defaulting.
fn load(args: &Options) -> Result<NanusConfig, String> {
    NanusConfig::load(args.config.as_deref()).map_err(|error| error.to_string())
}

/// Awaits the adapters one task needs.
async fn prepare_run(
    args: &Options,
    task: &[String],
    resume: Option<String>,
    name: Option<String>,
) -> Result<Ready, String> {
    let config = load(args)?;
    let prompt = task.join(" ");
    if prompt.trim().is_empty() {
        return Err(String::from(
            "the task is empty; pass the work to do, for example: nanus run \"summarize this repository\"",
        ));
    }

    // A name is claimed before anything is built or run. Checking here rather than after
    // the turn means a name somebody else holds costs a sentence instead of a turn, and
    // leaves no unnamed session behind as the evidence of it.
    if let Some(asked) = &name {
        let store = open_store().await.map_err(|error| error.to_string())?;
        if let Some(existing) = store
            .resolve(asked)
            .await
            .map_err(|error| error.to_string())?
        {
            return Err(format!(
                "the name {asked:?} already belongs to session {}",
                existing.as_str()
            ));
        }
    }

    // Awaiting here runs inside the runtime, which is the only place awaiting is legal.
    let pending = compose(&config).await.map_err(|error| error.to_string())?;
    let workspace = compose::workspace_root(&config).map_err(|error| error.to_string())?;
    Ok(Ready::Run {
        pending: Box::new(pending),
        workspace,
        prompt,
        verbose: args.verbose,
        resume,
        name,
    })
}

/// Decides how the interface will be started.
///
/// Reading a transcript and talking to an agent are different enough to be different
/// paths, and only one of them needs anything composed: a session that has already been
/// written down is a file, so `--session` composes no agent and reads no key.
// The three-state session flag is the parser's shape, carried here unchanged rather than
// flattened and re-derived: a `Some(None)` means "the most recent", and collapsing it to
// `None` on the way in would silently turn that into "no session at all".
#[allow(clippy::option_option, clippy::too_many_arguments)]
async fn prepare_tui(
    args: &Options,
    session: Option<Option<String>>,
    scroll: u32,
    connect: bool,
    socket: Option<PathBuf>,
    resume: Option<String>,
    name: Option<String>,
) -> Result<Ready, String> {
    let config = load(args)?;
    // Reading a transcript needs no agent and no key, so that mode stops here.
    if let Some(id) = session {
        let mut arguments: Vec<OsString> = vec![OsString::from("--session")];
        if let Some(id) = id {
            arguments.push(OsString::from(id));
        }
        if scroll > 0 {
            // Omitted when zero, because `--scroll 0` is the default and a needless
            // argument in the interface's own command line is a needless thing to read
            // in `ps`.
            arguments.push(OsString::from("--scroll"));
            arguments.push(OsString::from(scroll.to_string()));
        }
        return Ok(Ready::Spawn { arguments });
    }
    // Which conversation, carried through to the interface's own command line.
    let choice = conversation(resume, name);
    if !connect {
        let pending = compose(&config).await.map_err(|error| error.to_string())?;
        let workspace = compose::workspace_root(&config).map_err(|error| error.to_string())?;
        return Ok(Ready::Tui {
            pending: Box::new(pending),
            workspace,
            arguments: choice,
        });
    }
    // Connecting to a service needs no composition here: the agent is already running,
    // and this process is only going to hand its socket to the interface.
    let socket = crate::service::socket_path(&config, socket.as_deref())?;
    // Asked before the interface starts, so the answer is a sentence naming the fix rather
    // than an empty screen, and so no terminal is taken for an interface that would have
    // nothing to talk to.
    if let Err(error) = nanus_link::Client::connect(&socket).await {
        return Err(format!("{error}\nstart one with `nanus service start`"));
    }
    let mut arguments = vec![OsString::from("--link"), OsString::from(socket.as_os_str())];
    arguments.extend(choice);
    Ok(Ready::Spawn { arguments })
}

/// Builds the interface's instruction about which conversation to open.
///
/// Empty means a new, unnamed session, which is what the interface does when it is told
/// nothing.
fn conversation(resume: Option<String>, name: Option<String>) -> Vec<OsString> {
    match (resume, name) {
        (Some(reference), _) => vec![OsString::from("--resume"), OsString::from(reference)],
        (None, Some(name)) => vec![OsString::from("--name"), OsString::from(name)],
        (None, None) => Vec::new(),
    }
}

/// Decides what the service subcommand should do.
async fn prepare_service(args: &Options, action: ServiceAction) -> Result<Ready, String> {
    let config = load(args)?;
    match action {
        ServiceAction::Start {
            foreground,
            detached,
            socket,
            log,
        } => {
            let options = crate::service::Options {
                foreground,
                detached,
                socket,
                log,
                config_file: args.config.clone(),
            };
            match crate::service::start(&config, &options).await? {
                crate::service::Start::Started { socket } => {
                    println!("nanus: service listening on {}", socket.display());
                    Ok(Ready::Done)
                }
                crate::service::Start::Serve {
                    pending,
                    workspace,
                    socket,
                } => Ok(Ready::Serve {
                    pending,
                    workspace,
                    socket,
                }),
            }
        }
        ServiceAction::Stop { socket } => Ok(Ready::Stop {
            socket: crate::service::socket_path(&config, socket.as_deref())?,
        }),
        ServiceAction::Status { socket } => Ok(Ready::Status {
            socket: crate::service::socket_path(&config, socket.as_deref())?,
        }),
    }
}

/// Mounts a harness and runs one turn, synchronously.
///
/// Prints the answer on stdout and nothing else, persists the session, and tears the
/// composition down. Every step blocks rather than awaits, which is what keeps the
/// kernel's `block_on` from nesting inside a runtime.
fn run_turn(
    pending: compose::Pending,
    workspace: &Path,
    prompt: &str,
    verbose: bool,
    resume: Option<&str>,
    name: Option<&str>,
) -> Result<(), String> {
    let harness = pending.start().map_err(|error| error.to_string())?;
    // The session is resolved here rather than in `prepare`, because resolving it needs
    // the store the composition has just opened and loading it is a blocking call.
    let mut session = match resume {
        Some(reference) => load_session(&harness, reference)?,
        None => harness.new_session(workspace),
    };
    let mut reporter = StderrProgress::new(verbose, verbose);

    let outcome = kernel_block_on(harness.runner.run_turn(&mut session, prompt, &mut reporter));

    // Recorded whatever the turn did, and before anything is printed: a caller that
    // redirects stdout and loses the process should still find the transcript, and a turn
    // that *failed* is exactly the one worth being able to resume — what the model said
    // before it failed is what the next attempt has to work from.
    let recorded = record_after(&harness, &session, name);
    finish_harness(&harness)?;
    recorded?;

    let outcome = outcome.map_err(|error| error.to_string())?;

    // A trailing newline is the only decoration stdout gets, and it is there so the
    // answer is a line.
    print!("{}", outcome.answer);
    if !outcome.answer.ends_with('\n') {
        println!();
    }

    if outcome.is_success() {
        return Ok(());
    }
    // A run that did not complete is a failed run, and the reason goes to stderr so
    // stdout stays exactly the answer.
    Err(format!("the run did not complete: {:?}", outcome.reason))
}

/// Saves the session, and records its name when one was asked for.
///
/// One step rather than two because both are about the session being durable before the
/// caller is told anything, and because a named session that failed still has a name.
fn record_after(
    harness: &Harness,
    session: &nanus_domain::Session,
    name: Option<&str>,
) -> Result<(), String> {
    kernel_block_on(record(harness, session))?;
    let Some(name) = name else {
        return Ok(());
    };
    // After the save, because a name is an alias for a session that has to exist for the
    // alias to mean anything.
    kernel_block_on(harness.store.name(session.id(), name))
        .map_err(|error| format!("the session could not be named: {error}"))
}

/// Loads the session a reference names.
///
/// Driven with `block_on` from the synchronous half, because the caller is outside the
/// runtime by the time it runs.
fn load_session(harness: &Harness, reference: &str) -> Result<nanus_domain::Session, String> {
    let id = resolve_id(&harness.store, reference)?;
    kernel_block_on(harness.store.load(&id)).map_err(|error| error.to_string())
}

/// Resolves a reference that may be a name or a session id.
///
/// A name is tried first, because a name is the human-facing key: a session that both a
/// name and an id could refer to is one somebody named.
fn resolve_id(
    store: &nanus_ports::StoreHandle,
    reference: &str,
) -> Result<nanus_domain::SessionId, String> {
    match kernel_block_on(store.resolve(reference)) {
        Ok(Some(id)) => Ok(id),
        Ok(None) => Ok(nanus_domain::SessionId::new(reference)),
        Err(error) => Err(error.to_string()),
    }
}

/// Records a name against an existing session.
fn record_name(
    store: &nanus_ports::StoreHandle,
    name: &str,
    reference: &str,
) -> Result<(), String> {
    let id = resolve_id(store, reference)?;
    kernel_block_on(store.name(&id, name)).map_err(|error| error.to_string())?;
    // Naming an absent session fails above, so reaching here means both ends exist.
    println!("nanus: session {} is now called {name:?}", id.as_str());
    Ok(())
}

/// Persists the session, reporting a failure rather than losing it silently.
///
/// Awaited rather than driven with `block_on`: this already runs inside the runtime,
/// and starting one from within another panics.
async fn record(harness: &Harness, session: &nanus_domain::Session) -> Result<(), String> {
    harness
        .store
        .save(session)
        .await
        .map_err(|error| format!("the session could not be saved: {error}"))
}

/// Tears the composition down, reaping any process still running.
fn finish_harness(harness: &Harness) -> Result<(), String> {
    harness.shutdown().map_err(|error| error.to_string())
}

/// Prints the effective configuration.
fn show_config(args: &Options) -> Result<(), String> {
    let config = load(args)?;
    let path = NanusConfig::source_path(args.config.as_deref()).map_or_else(
        |_| String::from("<unavailable>"),
        |path| path.display().to_string(),
    );
    // Secrets are reported as present or absent, never printed: this output is
    // routinely pasted into an issue.
    let key = if nanus_adapter_config::api_key().is_some() {
        "set"
    } else {
        "not set"
    };
    println!("config file: {path}");
    println!("model: {}", config.model);
    println!("max tokens: {}", config.max_tokens);
    println!("reasoning effort: {:?}", config.reasoning_effort);
    println!("approval policy: {:?}", config.approval_policy);
    println!("sandbox mode: {:?}", config.sandbox_mode);
    println!("max steps per turn: {}", config.max_steps_per_turn);
    println!("max parallel tools: {}", config.max_parallel_tools);
    println!("workspace root: {}", workspace_display(&config));
    // Resolved rather than echoed: the socket is a path a user will paste into a command
    // or a supervisor, and a default spelled `<the nanus home>/…` is not one.
    println!(
        "service socket: {}",
        resolved(&crate::service::socket_path(&config, None))
    );
    println!(
        "service log: {}",
        resolved(&crate::service::log_path(&config, None))
    );
    println!("api key: {key}");
    Ok(())
}

/// Renders a resolved path, or names the home when it cannot be resolved.
fn resolved(path: &Result<PathBuf, String>) -> String {
    path.as_ref().map_or_else(
        |_| String::from("<the nanus home> is unavailable"),
        |path| path.display().to_string(),
    )
}

/// Renders the configured workspace root, or the default.
fn workspace_display(config: &NanusConfig) -> String {
    config.workspace_root.as_ref().map_or_else(
        || String::from("<the current directory>"),
        |root| root.display().to_string(),
    )
}

/// Awaits the session store, which needs no model.
///
/// No harness is composed: listing and naming sessions needs no model, so both work
/// without a configured key.
async fn prepare_sessions(action: Option<SessionsAction>) -> Result<Ready, String> {
    let store = open_store().await.map_err(|error| error.to_string())?;
    match action {
        None => Ok(Ready::List { store }),
        Some(SessionsAction::Name { name, session }) => Ok(Ready::Name {
            store,
            name,
            session,
        }),
    }
}

/// Prints the recorded sessions, newest first.
///
/// Driven with `block_on` from the synchronous half rather than awaited, because the
/// caller has already left the runtime by the time this runs.
fn print_sessions(store: &nanus_ports::StoreHandle) -> Result<(), String> {
    let listed = kernel_block_on(store.list()).map_err(|error| error.to_string())?;

    if listed.is_empty() {
        // Saying so is better than printing nothing, which looks like a failure.
        eprintln!("nanus: no sessions recorded");
        return Ok(());
    }
    for summary in listed {
        let title = summary.title.unwrap_or_else(|| String::from("<untitled>"));
        // The name is what a session is resumed by, so it goes first when there is one:
        // the id is what a script uses and the name is what a person types.
        let name = summary
            .name
            .map_or_else(String::new, |name| format!("[{name}]  "));
        println!(
            "{}  {name}{} events  {}  {title}",
            summary.id.as_str(),
            summary.event_count,
            summary.cwd
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parser_accepts_a_run_with_a_multi_word_task() {
        let args = Args::try_parse_from(["nanus", "run", "summarize", "this", "repo"]);
        assert!(args.is_ok());
        let Ok(args) = args else {
            return;
        };
        let Some(Command::Run { task, resume, name }) = args.command else {
            panic!("expected a run command");
        };
        assert_eq!(task, vec!["summarize", "this", "repo"]);
        assert_eq!(resume, None, "no --resume means a new session");
        assert_eq!(name, None, "no --name means an unnamed session");
    }

    #[test]
    fn the_run_takes_a_name_and_a_resume_but_not_both() {
        let named = Args::try_parse_from(["nanus", "run", "--name", "nightly", "do it"]);
        assert!(named.is_ok(), "{named:?}");
        let Ok(named) = named else { return };
        let Some(Command::Run { name, resume, .. }) = named.command else {
            panic!("expected a run command");
        };
        assert_eq!(name.as_deref(), Some("nightly"));
        assert_eq!(resume, None);

        let resumed = Args::try_parse_from(["nanus", "run", "--resume", "nightly", "do it"]);
        assert!(resumed.is_ok(), "{resumed:?}");
        // Naming and resuming are different intents for one conversation: one starts a
        // session and the other continues one, so asking for both is a mistake worth
        // refusing rather than a precedence rule to remember.
        assert!(
            Args::try_parse_from(["nanus", "run", "--name", "a", "--resume", "b", "x"]).is_err()
        );
    }

    #[test]
    fn the_interface_takes_a_name_and_a_resume() {
        let named = tui(&["nanus", "tui", "--name", "nightly"]);
        let Some(Command::Tui { name, resume, .. }) = Some(named) else {
            panic!("expected the tui command");
        };
        assert_eq!(name.as_deref(), Some("nightly"));
        assert_eq!(resume, None);

        let resumed = tui(&["nanus", "tui", "--resume", "nightly"]);
        let Some(Command::Tui { name, resume, .. }) = Some(resumed) else {
            panic!("expected the tui command");
        };
        assert_eq!(resume.as_deref(), Some("nightly"));
        assert_eq!(name, None);

        assert!(Args::try_parse_from(["nanus", "tui", "--name", "a", "--resume", "b"]).is_err());
        // Reading a transcript starts no session, so there is nothing to name or resume.
        assert!(Args::try_parse_from(["nanus", "tui", "--session", "--name", "a"]).is_err());
        assert!(Args::try_parse_from(["nanus", "tui", "--session", "--resume", "a"]).is_err());
    }

    #[test]
    fn the_conversation_the_interface_is_told_about_is_one_instruction() {
        // The interface is a separate program, so what it is asked for travels as its own
        // command line. Empty means a new unnamed session, which is what it does anyway.
        assert!(conversation(None, None).is_empty());
        assert_eq!(
            conversation(Some("nightly".to_owned()), None),
            vec![OsString::from("--resume"), OsString::from("nightly")]
        );
        assert_eq!(
            conversation(None, Some("nightly".to_owned())),
            vec![OsString::from("--name"), OsString::from("nightly")]
        );
    }

    #[test]
    fn the_sessions_command_lists_by_default_and_can_name() {
        let listed = Args::try_parse_from(["nanus", "sessions"]);
        assert!(listed.is_ok(), "{listed:?}");
        let Ok(listed) = listed else { return };
        let Some(Command::Sessions { action }) = listed.command else {
            panic!("expected the sessions command");
        };
        assert!(action.is_none(), "no action means the listing");

        let named = Args::try_parse_from(["nanus", "sessions", "name", "nightly", "01a09558"]);
        assert!(named.is_ok(), "{named:?}");
        let Ok(named) = named else { return };
        let Some(Command::Sessions {
            action: Some(SessionsAction::Name { name, session }),
        }) = named.command
        else {
            panic!("expected a name action");
        };
        assert_eq!(name, "nightly");
        assert_eq!(session, "01a09558");

        // Both ends are required: a name with nothing to name is not a command.
        assert!(Args::try_parse_from(["nanus", "sessions", "name", "nightly"]).is_err());
    }

    #[test]
    fn a_run_without_a_task_is_a_usage_error() {
        // Failing here rather than starting an empty turn is what makes a typo
        // obvious.
        assert!(Args::try_parse_from(["nanus", "run"]).is_err());
    }

    #[test]
    fn verbose_and_quiet_are_mutually_exclusive() {
        assert!(Args::try_parse_from(["nanus", "--verbose", "--quiet", "config"]).is_err());
        assert!(Args::try_parse_from(["nanus", "--verbose", "config"]).is_ok());
        assert!(Args::try_parse_from(["nanus", "--quiet", "config"]).is_ok());
    }

    #[test]
    fn no_subcommand_is_left_for_the_terminal_to_resolve() {
        // A bare `nanus` parses to *no* command rather than to one: whether it starts
        // the interface or prints the usage depends on there being a terminal, and that
        // is not something a parser should be deciding.
        let args = Args::try_parse_from(["nanus"]);
        assert!(args.is_ok());
        let Ok(args) = args else {
            return;
        };
        assert!(args.command.is_none());
    }

    /// Extracts the `tui` subcommand.
    fn tui(argv: &[&str]) -> Command {
        let parsed = Args::try_parse_from(argv);
        assert!(parsed.is_ok(), "{argv:?} should parse: {parsed:?}");
        let Ok(parsed) = parsed else {
            panic!("checked above");
        };
        let Some(command) = parsed.command else {
            panic!("expected a command");
        };
        command
    }

    #[test]
    fn ui_is_an_alias_for_tui() {
        // The interface is the thing most people want, and both spellings mean it.
        let by_alias = tui(&["nanus", "ui"]);
        let by_name = tui(&["nanus", "tui"]);
        assert!(matches!(by_alias, Command::Tui { .. }));
        assert!(matches!(by_name, Command::Tui { .. }));
    }

    #[test]
    fn the_tui_takes_an_optional_session_id() {
        let Some(Command::Tui {
            session, scroll, ..
        }) = Some(tui(&["nanus", "tui"]))
        else {
            panic!("expected the tui command");
        };
        assert_eq!(session, None, "no --session means a live conversation");
        assert_eq!(scroll, 0);

        let Some(Command::Tui { session, .. }) = Some(tui(&["nanus", "tui", "--session"])) else {
            panic!("expected the tui command");
        };
        // The outer `Some` is the flag having been given; the inner one is the id,
        // which is absent and therefore means "the most recent session".
        assert_eq!(session, Some(None));

        let Some(Command::Tui { session, .. }) =
            Some(tui(&["nanus", "tui", "--session", "01a09558"]))
        else {
            panic!("expected the tui command");
        };
        assert_eq!(session, Some(Some(String::from("01a09558"))));
    }

    #[test]
    fn a_flag_following_an_optional_value_is_not_swallowed_by_it() {
        // `--session --scroll 50` is the documented way to open the most recent session
        // part way back, and reading `--scroll` as the session id would break it.
        let Some(Command::Tui {
            session, scroll, ..
        }) = Some(tui(&["nanus", "tui", "--session", "--scroll", "50"]))
        else {
            panic!("expected the tui command");
        };
        assert_eq!(session, Some(None));
        assert_eq!(scroll, 50);
    }

    #[test]
    fn connecting_to_a_service_and_reading_a_session_are_mutually_exclusive() {
        // They are different modes, not two settings: one talks to an agent and the
        // other deliberately has none.
        assert!(Args::try_parse_from(["nanus", "tui", "--connect", "--session"]).is_err());
        assert!(Args::try_parse_from(["nanus", "tui", "--connect"]).is_ok());
    }

    #[test]
    fn a_socket_is_only_meaningful_when_connecting() {
        // `nanus tui` binds its own socket for the agent it starts, so a `--socket`
        // without `--connect` is a request that cannot be honoured. Refusing it is
        // better than ignoring it.
        assert!(Args::try_parse_from(["nanus", "tui", "--socket", "/tmp/s"]).is_err());
        let connected = Args::try_parse_from(["nanus", "tui", "--connect", "--socket", "/tmp/s"]);
        assert!(connected.is_ok(), "{connected:?}");
        let Ok(connected) = connected else { return };
        let Some(Command::Tui {
            socket, connect, ..
        }) = connected.command
        else {
            panic!("expected the tui command");
        };
        assert!(connect);
        assert_eq!(socket, Some(PathBuf::from("/tmp/s")));
    }

    #[test]
    fn the_service_takes_a_start_stop_and_status() {
        let started = Args::try_parse_from(["nanus", "service", "start", "--foreground"]);
        assert!(started.is_ok(), "{started:?}");
        let Ok(started) = started else { return };
        let Some(Command::Service {
            action: ServiceAction::Start { foreground, .. },
        }) = started.command
        else {
            panic!("expected a service start");
        };
        assert!(foreground);

        assert!(Args::try_parse_from(["nanus", "service", "stop"]).is_ok());
        assert!(Args::try_parse_from(["nanus", "service", "status", "--socket", "/tmp/s"]).is_ok());
    }

    #[test]
    fn a_service_with_no_action_is_a_usage_error() {
        // `nanus service` on its own would otherwise have to guess, and guessing between
        // "start" and "status" is how a typo becomes a running daemon.
        assert!(Args::try_parse_from(["nanus", "service"]).is_err());
    }

    #[test]
    fn the_hidden_detach_flag_is_accepted_but_not_advertised() {
        // The parent passes it; a person reading the help should not be invited to.
        assert!(Args::try_parse_from(["nanus", "service", "start", "--detached"]).is_ok());
        assert!(
            !help_text().contains("--detached"),
            "the internal flag is hidden:\n{}",
            help_text()
        );
    }

    #[test]
    fn the_usage_text_advertises_every_mode() {
        let help = help_text();
        for mode in ["run", "tui", "ui", "service", "config", "sessions"] {
            assert!(help.contains(mode), "the help omits {mode}:\n{help}");
        }
        // The default is the surprising part, so it has to be stated rather than left
        // for a user to discover by typing the program's name.
        assert!(
            help.contains("interactive"),
            "the help omits the default:\n{help}"
        );
    }

    #[test]
    fn the_config_path_is_global() {
        // A global option must work on either side of the subcommand, because users
        // reasonably write both.
        let before = Args::try_parse_from(["nanus", "--config", "/tmp/x.toml", "config"]);
        let after = Args::try_parse_from(["nanus", "config", "--config", "/tmp/x.toml"]);
        assert!(before.is_ok());
        assert!(after.is_ok());
    }

    #[test]
    fn help_and_version_are_successful_outcomes() {
        // `--help` exits the parse with an error type, and treating that as a failure
        // would make `nanus --help` exit non-zero.
        let help = Args::try_parse_from(["nanus", "--help"]);
        assert!(help.is_err());
        let Err(error) = help else {
            return;
        };
        assert!(!error.use_stderr(), "help is written to stdout");

        let version = Args::try_parse_from(["nanus", "--version"]);
        assert!(version.is_err());
    }

    #[test]
    fn the_workspace_display_names_the_default_rather_than_showing_nothing() {
        let config = NanusConfig::default();
        assert!(workspace_display(&config).contains("current directory"));
    }

    #[test]
    fn a_resolved_path_is_shown_and_an_unresolvable_one_is_named() {
        // The configuration prints paths a user will paste into a command, so a default
        // has to be shown resolved rather than as a placeholder, and a failure to resolve
        // has to say so rather than print nothing.
        assert_eq!(
            resolved(&Ok(PathBuf::from("/tmp/agent.sock"))),
            "/tmp/agent.sock"
        );
        assert!(resolved(&Err(String::from("no home"))).contains("nanus home"));
    }
}
