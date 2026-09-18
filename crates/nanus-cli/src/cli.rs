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
//! one, and `nanus sessions` lists, names, and deletes them. The name is an alias for the
//! store key, so a session keeps its identity through a rename, a name is refused rather
//! than moved when another session already answers to it, and deleting a session takes its
//! name with it.
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

use std::cell::Cell;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use clap::{CommandFactory, Parser, Subcommand};
use nanus_adapter_config::NanusConfig;
use nanus_bundle::compose::open_store;
use nanus_bundle::{Harness, Provider, Selection, compose};
use nanus_domain::ApprovalPolicy;
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

    /// Print nothing but the answer on stdout, which is the default.
    ///
    /// Accepted and deliberately without an effect of its own: stdout carries the answer and
    /// nothing else on every run, so this says out loud what the program already does, and a
    /// script that wants to state its expectation can. It conflicts with `--verbose` because
    /// the two ask for opposite things — one for progress on stderr, one for none of it —
    /// and a caller that asked for both has a mistake worth being told about rather than a
    /// precedence rule to remember.
    #[arg(long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// How a tool call outside the sandbox is approved at startup.
    ///
    /// `per_call` asks about every exception, `permitted` grants the non-destructive ones
    /// and asks about the rest, and `all_calls` grants every exception. Overrides the
    /// configuration file, and the interface can still cycle the state with Shift+Tab.
    #[arg(long, global = true, value_name = "STATE")]
    pub approval: Option<ApprovalPolicy>,

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

    /// Manage the provider credentials nanus reads.
    ///
    /// A credential is stored in the platform's own store — the macOS keychain, with a
    /// private file as the fallback — rather than in the configuration file or the
    /// environment, so it is not in every subprocess's environment and not in `ps`
    /// output. The environment variable remains the last fallback, for CI and
    /// containers.
    Auth {
        /// What to do with the credentials.
        #[command(subcommand)]
        action: AuthAction,
    },

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

    /// Remove a session and everything it recorded.
    ///
    /// The reference is resolved exactly as naming resolves it — a name it already answers
    /// to, then a store key — and a reference that answers to nothing is refused rather than
    /// reported as a deletion that removed nothing. Deleting is not reversible; the name, if
    /// the session had one, is released with it.
    Delete {
        /// The session to remove: an id, or a name it already answers to.
        session: String,
    },

    /// Report what a session did and what it spent.
    ///
    /// Read from the log rather than from an agent, so it needs no model and no key: the
    /// figures were written down as the turn ran. The reference resolves exactly as naming
    /// resolves one.
    Show {
        /// The session to report: an id, or a name it already answers to.
        session: String,

        /// Emit a JSON object instead of the readable report.
        ///
        /// For a script comparing runs, which is the reader this command exists for: the
        /// numbers a harness is judged by are the ones a script has to be able to read
        /// without parsing prose.
        #[arg(long)]
        json: bool,
    },
}

/// What to do with a stored credential.
#[derive(Debug, Subcommand)]
pub enum AuthAction {
    /// Store a provider credential, read from standard input.
    ///
    /// The value is read from standard input rather than taken as an argument, so it
    /// is never in this process's argument list for another process to read. Pipe it
    /// in (`printf %s "$KEY" | nanus auth set openai`) or type it and press enter;
    /// nothing is echoed in the second case, which is the same trade the platform's
    /// own tools make.
    Set {
        /// The provider the credential is for: one of the names `nanus config` lists.
        provider: String,
    },

    /// Remove a provider's stored credential.
    ///
    /// Removing one that is not stored is not an error; it is reported as having
    /// removed nothing.
    Clear {
        /// The provider whose credential is removed.
        provider: String,
    },

    /// Report which providers have a credential, and where they are read from.
    ///
    /// The credential itself is never printed: this output is routinely pasted into an
    /// issue.
    Status,
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
            // `use_stderr` is true for an error and false for a help or version request,
            // and it selects the *stream* as well as the outcome. Printing everything to
            // stdout put clap's "unexpected argument" and the whole usage block into a
            // redirected stdout — which is documented as the answer and nothing else, so a
            // script capturing an answer captured a usage message instead.
            if error.use_stderr() {
                eprint!("{rendered}");
                return Err(String::from("invalid arguments"));
            }
            print!("{rendered}");
            return Ok(Ready::Done);
        }
    };
    // The subcommand is taken by value so the task words move rather than clone, and
    // the global options are read from a separate borrow taken first.
    let Args {
        verbose,
        quiet: _,
        approval,
        config,
        command,
    } = args;
    let options = Options {
        verbose,
        approval,
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
        Command::Config => show_config(&options).await.map(|()| Ready::Done),
        Command::Auth { action } => prepare_auth(action).await,
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
    /// A session is ready to be removed.
    Delete {
        /// The store that holds it.
        store: nanus_ports::StoreHandle,
        /// The session to remove: an id, or a name it answers to.
        session: String,
    },
    /// A session is ready to be reported on.
    Show {
        /// The store that holds it.
        store: nanus_ports::StoreHandle,
        /// The session to report: an id, or a name it answers to.
        session: String,
        /// Whether to emit JSON rather than the readable report.
        json: bool,
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
        Ready::Delete { store, session } => delete_session(&store, &session),
        Ready::Show {
            store,
            session,
            json,
        } => print_session_report(&store, &session, json),
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

/// What a one-shot run calls itself in the claim it takes on its session.
///
/// A word for a person rather than for a program: the pid is in the claim already, and the
/// sentence another writer reads has to say *what* is holding the conversation.
const RUN_CLAIM: &str = "nanus run";

/// The global options, separated from the subcommand.
struct Options {
    verbose: bool,
    /// An approval state from the command line, which overrides the configuration file.
    approval: Option<ApprovalPolicy>,
    config: Option<PathBuf>,
}

/// Loads the configuration, reporting a malformed file rather than defaulting.
fn load(args: &Options) -> Result<NanusConfig, String> {
    let mut config =
        NanusConfig::load(args.config.as_deref()).map_err(|error| error.to_string())?;
    // The flag wins over the file, because it is the more specific instruction: a person who
    // typed `--approval all_calls` on this command meant it for this run.
    if let Some(approval) = args.approval {
        config.approval_policy = approval;
    }
    Ok(config)
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
    // `--scroll` addresses a recorded transcript, and a live conversation has nothing to open
    // part way into. Refused rather than ignored: a reader who asked to open fifty rows back
    // and got the bottom of the conversation has been told something untrue by silence.
    if scroll > 0 && session.is_none() {
        return Err(String::from(
            "--scroll opens a recorded session part way back, so it needs --session",
        ));
    }
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
        return Ok(Ready::Spawn {
            arguments: with_approval(arguments, args.approval),
        });
    }
    // Which conversation, carried through to the interface's own command line.
    let choice = with_approval(conversation(resume, name), args.approval);
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
    // nothing to talk to. Only "nothing is listening" gets the start hint: a version
    // mismatch is a different problem with its own sentence.
    match nanus_link::Client::connect(&socket).await {
        Ok(_) => {}
        Err(nanus_link::LinkError::Connect { .. }) => {
            return Err(format!(
                "no agent is listening at {}\nstart one with `nanus service start`",
                socket.display()
            ));
        }
        Err(error) => return Err(error.to_string()),
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

/// Adds the interface's own `--approval` instruction to its command line.
///
/// The interface is a separate program, so a state named at startup travels as an argument
/// like everything else it is told. It is passed even for an agent this process started —
/// the agent already has the state, and the interface draws it immediately rather than
/// waiting for the frame that would follow the attachment.
fn with_approval(mut arguments: Vec<OsString>, approval: Option<ApprovalPolicy>) -> Vec<OsString> {
    if let Some(approval) = approval {
        arguments.push(OsString::from("--approval"));
        arguments.push(OsString::from(approval.as_str()));
    }
    arguments
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

/// Sets `stop` when the process is interrupted.
///
/// Resolves when the signal arrives, or immediately when it cannot be watched for at all — a
/// platform without the signal, in which case the turn simply runs to completion as it did
/// before. Nothing here prints: what the reader sees is the turn's own ending.
async fn watch_for_interrupt(stop: Rc<Stop>) {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut interrupt) = signal(SignalKind::interrupt()) else {
        return;
    };
    interrupt.recv().await;
    stop.raise();
}

/// The interrupt a headless run watches for.
///
/// Two shapes of one fact, because two things wait on it differently: the turn *polls* it
/// between steps and between tokens, which is a `Cell`, and an open approval question is
/// *asleep* in a blocking read, which needs a wake-up. Both are raised together so the key
/// means one thing.
#[derive(Default)]
struct Stop {
    /// Polled by the turn through its reporter.
    raised: Rc<Cell<bool>>,
    /// Woken by the interrupt, so a question in progress is abandoned rather than answered.
    woken: Rc<tokio::sync::Notify>,
}

impl Stop {
    /// Raises the stop: the turn learns at its next checkpoint, and a question that is open
    /// right now stops waiting for an answer nobody is going to give.
    fn raise(&self) {
        self.raised.set(true);
        // `notify_one` rather than `notify_waiters`: a permit is stored if nobody is
        // waiting yet, so an interrupt that arrives a moment before the question opens is
        // not lost.
        self.woken.notify_one();
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
    // Claimed for the whole run: this is the window in which an agent answering a *different*
    // command could overwrite the same log, and a session two writers share is a conversation
    // that loses a turn. A refusal means somebody else is holding it — most often a service —
    // so the sentence says what to do instead of just what happened.
    let id = session.id().clone();
    kernel_block_on(harness.store.lock(&id, RUN_CLAIM))
        .map_err(|error| format!("{error}; attach to it with `nanus tui --connect`"))?;
    // A headless run is the one mode with no interface to press a key in, so the interrupt is
    // watched for here and handed to the turn through its reporter: it stops at the next
    // checkpoint, the session is recorded, and the exit code says the turn did not complete.
    let stop = Rc::new(Stop::default());
    let mut reporter = StderrProgress::new(verbose, verbose).stopping_when(Rc::clone(&stop.raised));
    // A headless run is the one mode with no interface to press a key in, so the approval
    // gate asks *here*. With no terminal on stdin there is nobody to ask, and the loop
    // denies what the sandbox does not already permit rather than proceeding unasked. An
    // interrupt abandons a question that is open, so `Ctrl-C` stops a run that is waiting to
    // be asked something rather than having to be answered first.
    let approver =
        crate::approve::TerminalApprover::standard().abandoned_when(Rc::clone(&stop.woken));

    let outcome = crate::block_on_local(async {
        // The watcher is a task of its own rather than half of a `join!`. Joining made the
        // run wait for *both*, and the watcher only ever finishes when a signal arrives — so
        // `nanus run` printed nothing and exited only after a Ctrl-C, which is not a
        // behaviour anyone could mistake for a long model call. The turn is the thing being
        // awaited; the watcher exists to set the flag the turn reads at its next checkpoint.
        tokio::task::spawn_local(watch_for_interrupt(Rc::clone(&stop)));
        harness
            .runner
            .run_turn(&mut session, prompt, &mut reporter, Some(&approver))
            .await
    });

    // Recorded whatever the turn did, and before anything is printed: a caller that
    // redirects stdout and loses the process should still find the transcript, and a turn
    // that *failed* is exactly the one worth being able to resume — what the model said
    // before it failed is what the next attempt has to work from.
    let recorded = record_after(&harness, &session, name);
    // Released before the harness is torn down, and after the log has been written: the claim
    // covers exactly the time this process could have written it.
    harness.store.release_lock(&id);
    finish_harness(&harness)?;
    recorded?;

    let outcome = outcome.map_err(|error| error.to_string())?;

    // A trailing newline is the only decoration stdout gets, and it is there so the
    // answer is a line.
    print!("{}", outcome.answer);
    if !outcome.answer.ends_with('\n') {
        println!();
    }

    // The totals, on stderr and only when progress was asked for: stdout stays exactly the
    // answer, and a caller who did not ask to watch the run did not ask to be told about it
    // either. Printed before the failure branch, because a run that stopped early is exactly
    // the one whose cost somebody wants to see.
    if verbose {
        eprintln!("{}", run_summary(outcome.steps, &outcome.usage));
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

/// Removes a session, refusing a reference that answers to nothing.
///
/// The reference is resolved the way naming resolves one — a name it already answers to
/// first, then a store key — and the session is checked to exist before anything is removed.
/// The store's `delete` deliberately treats an absent session as success (the caller asked
/// for it to be gone and it is), so the refusal has to be here: `nanus sessions delete typo`
/// reporting that it deleted something is a person believing a conversation is gone when it
/// is still on disk, which is the one thing a delete must not do.
fn delete_session(store: &nanus_ports::StoreHandle, reference: &str) -> Result<(), String> {
    let id = resolve_id(store, reference)?;
    let listed = kernel_block_on(store.list()).map_err(|error| error.to_string())?;
    let Some(summary) = listed.into_iter().find(|summary| summary.id == id) else {
        return Err(format!("no session answers to {reference:?}"));
    };
    kernel_block_on(store.delete(&id)).map_err(|error| error.to_string())?;
    // The name goes with the session, so saying which one it was is the last chance to
    // notice that the wrong conversation was removed.
    let name = summary
        .name
        .map_or_else(String::new, |name| format!(" ({name})"));
    println!("nanus: deleted session {}{name}", id.as_str());
    Ok(())
}

/// The one-line totals a watched run ends with.
///
/// The same figures `nanus sessions show` reports afterwards, laid out for somebody
/// watching rather than for a script — which is the whole difference between the two
/// commands, and not a difference in what they count.
fn run_summary(steps: u32, usage: &nanus_domain::Usage) -> String {
    format!(
        "nanus: {steps} steps \u{b7} {} prompt ({} cached, {} read) + {} generated ({} thinking)",
        usage.prompt_tokens,
        usage.cache_hit_tokens,
        usage.cache_miss_tokens,
        usage.completion_tokens,
        usage.reasoning_tokens
    )
}

/// Reports what a recorded session did and what it spent.
///
/// Everything here is read from the log, so it needs no model and no API key — the numbers
/// were written down as the turn ran, and reading them back is a file operation. That is
/// what makes this the command a comparison between two runs is built from: the same session
/// reported twice gives the same figures, whether the run happened a minute ago or last week.
fn print_session_report(
    store: &nanus_ports::StoreHandle,
    reference: &str,
    json: bool,
) -> Result<(), String> {
    let id = resolve_id(store, reference)?;
    // Loaded rather than summarised: the totals live in the events, and a summary carries
    // only what a picker needs to draw a row.
    let session = kernel_block_on(store.load(&id)).map_err(|error| error.to_string())?;
    // The name is a separate file beside the log, so it is read from the listing. A failure
    // to read it costs the name and not the report, which is why it is not a `?`.
    let name = kernel_block_on(store.list()).ok().and_then(|listed| {
        listed
            .into_iter()
            .find(|summary| summary.id == id)
            .and_then(|summary| summary.name)
    });

    if json {
        println!("{}", session_report_json(&session, name.as_deref()));
        return Ok(());
    }
    print!("{}", session_report_text(&session, name.as_deref()));
    Ok(())
}

/// Renders the report as a JSON object.
///
/// Field names are the ones the log uses, so a reader who has a session file open and a
/// reader who has this output agree on what each number is called.
fn session_report_json(session: &nanus_domain::Session, name: Option<&str>) -> String {
    let usage = session.usage_totals();
    let origin = session.origin();
    let value = serde_json::json!({
        "session": session.id().as_str(),
        "name": name,
        "created_at_ms": session.created_at_ms(),
        "workspace": session.cwd(),
        "origin": origin,
        "turns": session.turn_count(),
        "steps": session.step_count(),
        "requests": session.request_count(),
        "ended": session.log().last_turn_end().map(nanus_domain::TurnEndReason::label),
        "usage": {
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "reasoning_tokens": usage.reasoning_tokens,
            "cache_hit_tokens": usage.cache_hit_tokens,
            "cache_miss_tokens": usage.cache_miss_tokens,
            "total_tokens": usage.total_tokens(),
        },
        "by_model": session
            .usage_by_model()
            .iter()
            .map(|(model, totals)| serde_json::json!({
                "model": model,
                "prompt_tokens": totals.prompt_tokens,
                "completion_tokens": totals.completion_tokens,
                "reasoning_tokens": totals.reasoning_tokens,
                "cache_hit_tokens": totals.cache_hit_tokens,
                "cache_miss_tokens": totals.cache_miss_tokens,
                "total_tokens": totals.total_tokens(),
            }))
            .collect::<Vec<_>>(),
    });
    // A value that cannot encode would be a bug in the shape above rather than bad input,
    // so the fallback is an empty object rather than a panic in a reporting command.
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| String::from("{}"))
}

/// Renders the report as the rows a person reads.
///
/// Deliberately not the interface's `/stats` panel even though the two overlap: the panel
/// reports a *live* request, so it has figures the log never recorded — time to first token,
/// a decode rate — while this has history the panel does not.
fn session_report_text(session: &nanus_domain::Session, name: Option<&str>) -> String {
    let usage = session.usage_totals();
    let row = |label: &str, reading: String| format!("  {label:<11} {reading}");
    let mut lines: Vec<String> = vec![String::from("session report")];

    let name = name.map_or_else(String::new, |name| format!("  [{name}]"));
    lines.push(row("session", format!("{}{name}", session.id().as_str())));
    lines.push(row(
        "created",
        session
            .created_at_rfc3339()
            .unwrap_or_else(|| format!("{} (ms since the epoch)", session.created_at_ms())),
    ));
    lines.push(row("workspace", session.cwd().to_owned()));
    lines.push(row("origin", origin_line(session.origin())));
    lines.push(row(
        "activity",
        format!(
            "{} turns · {} steps · {} requests",
            session.turn_count(),
            session.step_count(),
            session.request_count()
        ),
    ));
    lines.push(row(
        "prompt",
        format!(
            "{} tokens · {} cached, {} read · {} hit",
            usage.prompt_tokens,
            usage.cache_hit_tokens,
            usage.cache_miss_tokens,
            share(usage.cache_hit_tokens, usage.prompt_tokens)
        ),
    ));
    lines.push(row(
        "generated",
        format!(
            "{} tokens · {} thinking",
            usage.completion_tokens,
            share(usage.reasoning_tokens, usage.completion_tokens)
        ),
    ));
    // Only worth a row when there is more than one, which is the case a resumed session
    // changes the model in: one line per model is what makes the two comparable.
    let by_model = session.usage_by_model();
    if by_model.len() > 1 {
        let models: Vec<String> = by_model
            .iter()
            .map(|(model, totals)| {
                row(
                    "  model",
                    format!(
                        "{} · {} prompt ({} cached) + {} generated",
                        model.as_deref().unwrap_or("<not recorded>"),
                        totals.prompt_tokens,
                        totals.cache_hit_tokens,
                        totals.completion_tokens
                    ),
                )
            })
            .collect();
        lines.extend(models);
    }
    lines.push(row(
        "ended",
        session.log().last_turn_end().map_or_else(
            || String::from("<still open>"),
            |reason| reason.label().to_owned(),
        ),
    ));
    lines.join("\n") + "\n"
}

/// Renders the harness configuration a session recorded.
///
/// A session recorded before any of this existed has nothing to show, and saying so is the
/// point: reporting a default here would attribute the run to a configuration nobody chose.
fn origin_line(origin: Option<&nanus_domain::Origin>) -> String {
    let Some(origin) = origin else {
        return String::from("<not recorded>");
    };
    let field = |label: &str, value: Option<&str>| {
        value.map_or_else(
            || format!("{label} <not recorded>"),
            |value| format!("{label} {value}"),
        )
    };
    [
        field("model", origin.model.as_deref()),
        field("effort", origin.effort.as_deref()),
        field("sandbox", origin.sandbox.as_deref()),
        field("approval", origin.approval.as_deref()),
        field("harness", origin.harness.as_deref()),
    ]
    .join(" · ")
}

/// Renders `part` as a percentage of `whole`, or a dash when there is no whole.
fn share(part: u32, whole: u32) -> String {
    u64::from(part)
        .checked_mul(100)
        .and_then(|scaled| scaled.checked_div(u64::from(whole)))
        .map_or_else(|| String::from("-"), |percent| format!("{percent}%"))
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

/// Stores, removes, or reports the provider credentials.
///
/// Needs no model and no session store, so it works before anything is configured —
/// which is the point: the first thing a new install does is store a key.
async fn prepare_auth(action: AuthAction) -> Result<Ready, String> {
    let secrets = compose::open_secrets().map_err(|error| error.to_string())?;
    match action {
        AuthAction::Set { provider } => {
            let account = provider_account(&provider)?;
            let credential = read_credential().await?;
            secrets
                .set(account, &credential)
                .await
                .map_err(|error| error.to_string())?;
            // Which store answered is reported, because a fallback that is in use is
            // something a reader should know rather than discover.
            println!(
                "nanus: stored a credential for {account} ({})",
                secrets.backend()
            );
            Ok(Ready::Done)
        }
        AuthAction::Clear { provider } => {
            let account = provider_account(&provider)?;
            let removed = secrets
                .clear(account)
                .await
                .map_err(|error| error.to_string())?;
            if removed {
                println!("nanus: removed the stored credential for {account}");
            } else {
                // The environment cannot be changed from here, so a reader whose key
                // comes from a variable is told where to remove it rather than being
                // left with a credential this command cannot see.
                let provider = Provider::parse(account).map_or("", Provider::env_var);
                println!(
                    "nanus: no stored credential for {account}; if it comes from {provider}, unset it in the shell that sets it"
                );
            }
            Ok(Ready::Done)
        }
        AuthAction::Status => {
            println!("credential stores: {}", secrets.backend());
            for provider in Provider::ALL {
                let account = provider.name();
                // The credential is never printed: this output is routinely pasted
                // into an issue. A store that could not answer says so rather than
                // reporting an absence it did not observe.
                let state = match secrets.get(account).await {
                    Ok(Some(credential)) if !credential.is_blank() => String::from("set"),
                    Ok(_) => String::from("not set"),
                    Err(error) => format!("not readable ({error})"),
                };
                println!("  {account}: {state}  (fallback {})", provider.env_var());
            }
            Ok(Ready::Done)
        }
    }
}

/// Resolves a provider name from the command line, or refuses it by name.
///
/// The account a credential is filed under is the provider's name, so the name is
/// checked against the providers this build actually has: storing a key under a
/// misspelling would be a credential nothing ever reads.
fn provider_account(asked: &str) -> Result<&'static str, String> {
    Provider::parse(asked).map(Provider::name).ok_or_else(|| {
        format!(
            "unknown provider {asked:?}: this build offers {}",
            Provider::names().join(", ")
        )
    })
}

/// Reads one credential from standard input.
///
/// Standard input rather than an argument, so the value never appears in this
/// process's argument list — and read asynchronously, because this runs inside the
/// runtime.
async fn read_credential() -> Result<String, String> {
    use tokio::io::AsyncBufReadExt as _;
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let line = lines
        .next_line()
        .await
        .map_err(|error| format!("could not read standard input: {error}"))?;
    let Some(line) = line else {
        return Err(String::from(
            "no credential was given on standard input; pipe one in or type it and press enter",
        ));
    };
    let credential = line.trim().to_owned();
    if credential.is_empty() {
        return Err(String::from("the credential is empty"));
    }
    Ok(credential)
}

/// Prints the effective configuration.
async fn show_config(args: &Options) -> Result<(), String> {
    let config = load(args)?;
    // Resolved rather than echoed: the file may name none of the provider, plan, or
    // model, and what a run will actually use is the useful answer. A configuration
    // that cannot resolve is refused here, with the same sentence a run would give.
    let selection = Selection::resolve(&config).map_err(|error| error.to_string())?;
    let path = NanusConfig::source_path(args.config.as_deref()).map_or_else(
        |_| String::from("<unavailable>"),
        |path| path.display().to_string(),
    );
    let credential = credential_state(&selection).await;
    println!("config file: {path}");
    println!(
        "provider: {} (plan {})",
        selection.provider(),
        selection.plan().name
    );
    println!("model: {}", selection.model());
    println!("endpoint: {}", selection.endpoint());
    println!("max tokens: {}", max_tokens_display(&config, &selection));
    if selection.provider().effort_applies() {
        println!("reasoning effort: {:?}", config.reasoning_effort);
    } else {
        // An inert knob is named as inert rather than printed as though it were sent.
        println!(
            "reasoning effort: {:?} (not sent to {})",
            config.reasoning_effort,
            selection.provider()
        );
    }
    println!("approval policy: {}", config.approval_policy);
    println!("sandbox mode: {:?}", config.sandbox_mode);
    println!("max steps per turn: {}", config.max_steps_per_turn);
    println!("max parallel tools: {}", config.max_parallel_tools);
    println!("tui detail: {}", config.tui_detail);
    println!("markdown answers: {}", config.markdown);
    println!("mermaid diagrams: {}", config.mermaid);
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
    println!(
        "credential: {credential} for {} (fallback {})",
        selection.credential_account(),
        selection.credential_env()
    );
    Ok(())
}

/// Reports whether a credential is present, without printing it.
async fn credential_state(selection: &Selection) -> String {
    let Ok(secrets) = compose::open_secrets() else {
        return String::from("unknown (the credential stores are unavailable)");
    };
    match secrets.get(selection.credential_account()).await {
        Ok(Some(credential)) if !credential.is_blank() => String::from("set"),
        Ok(_) => String::from("not set"),
        Err(error) => format!("not readable ({error})"),
    }
}

/// Renders the token budget, naming the provider's ceiling when it caps it.
fn max_tokens_display(config: &NanusConfig, selection: &Selection) -> String {
    let ceiling = selection.max_output_tokens();
    if config.max_tokens > ceiling {
        return format!(
            "{} (capped to {ceiling} by {})",
            config.max_tokens,
            selection.provider()
        );
    }
    format!("{} (ceiling {ceiling})", config.max_tokens)
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
        Some(SessionsAction::Delete { session }) => Ok(Ready::Delete { store, session }),
        Some(SessionsAction::Show { session, json }) => Ok(Ready::Show {
            store,
            session,
            json,
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

        let deleted = Args::try_parse_from(["nanus", "sessions", "delete", "nightly"]);
        assert!(deleted.is_ok(), "{deleted:?}");
        let Ok(deleted) = deleted else { return };
        let Some(Command::Sessions {
            action: Some(SessionsAction::Delete { session }),
        }) = deleted.command
        else {
            panic!("expected a delete action");
        };
        assert_eq!(session, "nightly");
        // A deletion names exactly one session: there is nothing to ask for twice.
        assert!(Args::try_parse_from(["nanus", "sessions", "delete"]).is_err());
        assert!(
            Args::try_parse_from(["nanus", "sessions", "delete", "a", "b"]).is_err(),
            "a second reference is a usage error rather than a silent second deletion"
        );

        let shown = Args::try_parse_from(["nanus", "sessions", "show", "nightly"]);
        assert!(shown.is_ok(), "{shown:?}");
        let Ok(shown) = shown else { return };
        let Some(Command::Sessions {
            action: Some(SessionsAction::Show { session, json }),
        }) = shown.command
        else {
            panic!("expected a show action");
        };
        assert_eq!(session, "nightly");
        assert!(!json, "the readable report is the default");

        let as_json = Args::try_parse_from(["nanus", "sessions", "show", "--json", "nightly"]);
        assert!(as_json.is_ok(), "{as_json:?}");
        let Ok(as_json) = as_json else { return };
        let Some(Command::Sessions {
            action: Some(SessionsAction::Show { json, .. }),
        }) = as_json.command
        else {
            panic!("expected a show action");
        };
        assert!(json, "--json asks for the machine-readable shape");
        assert!(Args::try_parse_from(["nanus", "sessions", "show"]).is_err());
    }

    /// A session with one answered turn, for the report renderers.
    fn reported_session() -> nanus_domain::Session {
        use nanus_domain::{Origin, SessionEvent, SessionId, Usage};

        let mut session =
            nanus_domain::Session::new(SessionId::new("s-1"), 1_700_000_000_000, "/work")
                .with_origin(Origin {
                    model: Some("deepseek-flash".to_owned()),
                    effort: Some("medium".to_owned()),
                    sandbox: Some("read_only".to_owned()),
                    approval: Some("per_call".to_owned()),
                    harness: Some("nanus/0.1.0".to_owned()),
                });
        session.append(SessionEvent::TurnStart { turn: 0 });
        session.append(SessionEvent::StepStart { turn: 0, step: 0 });
        session.append(SessionEvent::UserMessage {
            text: "read the file".to_owned(),
        });
        session.append(SessionEvent::AssistantMessage {
            text: Some(String::from("done")),
            reasoning: None,
            tool_calls: Vec::new(),
            usage: Some(Usage::new(1_000, 100, 40, 800, 200)),
            interrupted: false,
            model: Some("deepseek-flash".to_owned()),
            effort: Some("medium".to_owned()),
        });
        session.append(SessionEvent::StepEnd { turn: 0, step: 0 });
        session.append(SessionEvent::TurnEnd {
            turn: 0,
            reason: nanus_domain::TurnEndReason::Completed,
        });
        session
    }

    #[test]
    fn the_report_states_the_configuration_and_the_cost() {
        // The two figures the command exists for: what produced the run, and what it spent.
        // Both are read back from the log, so a report is comparable between two runs.
        let session = reported_session();
        let text = session_report_text(&session, Some("nightly"));
        assert!(text.contains("deepseek-flash"), "{text}");
        assert!(text.contains("medium"), "the effort is reported: {text}");
        assert!(text.contains("1 turns"), "{text}");
        assert!(text.contains("[nightly]"), "{text}");
        assert!(
            text.contains("800 cached, 200 read"),
            "the cache split is the cost driver and has to be visible: {text}"
        );
        assert!(text.contains("80% hit"), "{text}");
        assert!(text.contains("completed"), "{text}");
    }

    #[test]
    fn the_json_report_carries_the_same_figures_as_the_text_one() {
        let session = reported_session();
        let raw = session_report_json(&session, Some("nightly"));
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).expect("the report is valid JSON");
        assert_eq!(parsed["session"], serde_json::json!("s-1"));
        assert_eq!(parsed["name"], serde_json::json!("nightly"));
        assert_eq!(
            parsed["origin"]["model"],
            serde_json::json!("deepseek-flash")
        );
        assert_eq!(parsed["origin"]["effort"], serde_json::json!("medium"));
        assert_eq!(parsed["turns"], serde_json::json!(1));
        assert_eq!(parsed["usage"]["prompt_tokens"], serde_json::json!(1_000));
        assert_eq!(parsed["usage"]["cache_hit_tokens"], serde_json::json!(800));
        assert_eq!(parsed["ended"], serde_json::json!("completed"));
        assert_eq!(
            parsed["by_model"][0]["model"],
            serde_json::json!("deepseek-flash")
        );
    }

    #[test]
    fn a_session_that_recorded_no_origin_says_so_rather_than_guessing() {
        // The reading this must not produce is a plausible default: attributing a run to a
        // configuration nobody chose is worse than admitting the gap.
        let bare = nanus_domain::Session::new(
            nanus_domain::SessionId::new("s-2"),
            1_700_000_000_000,
            "/work",
        );
        assert!(session_report_text(&bare, None).contains("<not recorded>"));
        let parsed: serde_json::Value =
            serde_json::from_str(&session_report_json(&bare, None)).expect("valid JSON");
        assert_eq!(parsed["origin"], serde_json::Value::Null);
    }

    #[test]
    fn a_share_with_no_whole_is_a_dash_rather_than_a_division() {
        assert_eq!(share(0, 0), "-");
        assert_eq!(share(5, 0), "-");
        assert_eq!(share(1, 4), "25%");
        assert_eq!(share(4, 4), "100%");
    }

    #[test]
    fn the_run_summary_carries_the_cache_split_not_only_a_total() {
        // A prompt total on its own cannot tell a reader whether the run was cheap: the
        // cache split is the difference between a prefix the provider had already read and
        // one it had to read again.
        let usage = nanus_domain::Usage::new(12_000, 900, 300, 10_500, 1_500);
        let summary = run_summary(7, &usage);
        assert!(summary.contains("7 steps"), "{summary}");
        assert!(summary.contains("12000 prompt"), "{summary}");
        assert!(summary.contains("10500 cached, 1500 read"), "{summary}");
        assert!(
            summary.contains("900 generated (300 thinking)"),
            "{summary}"
        );
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

    /// A scroll offset is meaningless without a recording, and the parser is not the place to
    /// decide it: `--scroll` has a default, so clap cannot tell "not given" from "given as
    /// zero". The refusal is therefore in the mode's own preparation.
    #[tokio::test]
    async fn scrolling_without_a_session_is_refused() {
        // `/definitely/not/a/directory` would fail for a different reason, so the refusal is
        // asserted on the message rather than on the error being present.
        let refused = prepare_tui(
            &Options {
                verbose: false,
                approval: None,
                config: None,
            },
            None,
            50,
            false,
            None,
            None,
            None,
        )
        .await;
        match refused {
            Err(message) => assert!(message.contains("--scroll"), "{message}"),
            Ok(_) => panic!("a scroll offset without --session must be refused"),
        }
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

    /// A store in a temporary home, so a delete test never touches a real session.
    fn store_under(home: &Path) -> nanus_ports::StoreHandle {
        use nanus_adapter_store::JsonlStore;
        kernel_block_on(JsonlStore::new(home.to_path_buf()))
            .unwrap_or_else(|error| panic!("a store in the temporary directory: {error}"))
            .handle()
    }

    /// Records one session, named if asked, and returns its id.
    fn saved_session(
        store: &nanus_ports::StoreHandle,
        id: &str,
        name: Option<&str>,
    ) -> nanus_domain::SessionId {
        let session = nanus_domain::Session::new(nanus_domain::SessionId::new(id), 0, "/work");
        let session_id = session.id().clone();
        kernel_block_on(store.save(&session))
            .unwrap_or_else(|error| panic!("the session is saved: {error}"));
        if let Some(name) = name {
            kernel_block_on(store.name(&session_id, name))
                .unwrap_or_else(|error| panic!("the session is named: {error}"));
        }
        session_id
    }

    #[test]
    fn deleting_a_session_by_name_removes_it_and_releases_the_name() {
        let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
        let store = store_under(dir.path());
        let id = saved_session(&store, "01a09558", Some("nightly"));

        let deleted = delete_session(&store, "nightly");
        assert!(deleted.is_ok(), "deleting by name works: {deleted:?}");

        // The directory is gone, so a listing no longer shows it...
        let listed = kernel_block_on(store.list()).unwrap_or_else(|error| panic!("{error}"));
        assert!(listed.is_empty(), "nothing is left: {listed:?}");
        // ...and the name is released with it, so a later session cannot inherit an alias
        // for a conversation that no longer exists.
        let resolved =
            kernel_block_on(store.resolve("nightly")).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(resolved, None, "the name was released");
        // The id still resolves to itself as a name, which finds nothing now.
        assert!(
            delete_session(&store, id.as_str()).is_err(),
            "the id is gone too"
        );
    }

    #[test]
    fn deleting_a_session_that_is_not_there_is_refused() {
        let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
        let store = store_under(dir.path());
        let _kept = saved_session(&store, "01a09558", None);

        let refused = delete_session(&store, "ghost");
        let Err(message) = refused else {
            panic!("an unknown reference must be refused: {refused:?}");
        };
        assert!(message.contains("ghost"), "the refusal names it: {message}");

        // And the refusal removed nothing: the session that *is* there is still there.
        let listed = kernel_block_on(store.list()).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(listed.len(), 1, "the store was not touched: {listed:?}");
    }
}
