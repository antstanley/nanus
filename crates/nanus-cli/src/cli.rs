//! Argument parsing and the three modes.
//!
//! The command surface is deliberately small. A harness that grows a subcommand per
//! feature becomes a language of its own, and every one of them is a thing a user
//! has to learn before the tool does anything.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use nanus_adapter_config::NanusConfig;
use nanus_bundle::compose::open_store;
use nanus_bundle::{Harness, compose};
use nanus_kernel::runtime::block_on as kernel_block_on;

use crate::progress::StderrProgress;

/// A coding agent harness.
#[derive(Debug, Parser)]
#[command(
    name = "nanus",
    version,
    about = "A coding agent harness",
    long_about = "A coding agent harness built on a Rust implementation of the Cordis \
                  meta-framework. Run one task and print its answer, start an interactive \
                  session, or inspect the configuration and sessions on this machine."
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
        /// The task, as one or more words.
        #[arg(required = true, value_name = "TASK")]
        task: Vec<String>,
    },

    /// Show the effective configuration.
    Config,

    /// List recorded sessions, newest first.
    Sessions,
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
            // A help or version request is a successful outcome with nothing left to
            // do; a usage error is a failure.
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
    let options = Options { verbose, config };
    match command.unwrap_or(Command::Config) {
        Command::Run { task } => prepare_run(&options, &task).await,
        // Showing the configuration prints and is finished.
        Command::Config => show_config(&options).map(|()| Ready::Done),
        Command::Sessions => prepare_list().await,
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
    },
    /// The session store is open and its contents are ready to read.
    List {
        /// The store to read from.
        store: nanus_ports::StoreHandle,
    },
}

/// Completes the work [`prepare`] set up, synchronously.
///
/// Deliberately **not** `async`: every step here either mounts a kernel or drives the
/// agent loop with `block_on`, and both are illegal inside a runtime.
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
        } => run_turn(*pending, &workspace, &prompt, verbose),
        Ready::List { store } => print_sessions(&store),
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
async fn prepare_run(args: &Options, task: &[String]) -> Result<Ready, String> {
    let config = load(args)?;
    let prompt = task.join(" ");
    if prompt.trim().is_empty() {
        return Err(String::from(
            "the task is empty; pass the work to do, for example: nanus run \"summarize this repository\"",
        ));
    }

    // Awaiting here runs inside the runtime, which is the only place awaiting is legal.
    let pending = compose(&config).await.map_err(|error| error.to_string())?;
    let workspace = compose::workspace_root(&config).map_err(|error| error.to_string())?;
    Ok(Ready::Run {
        pending: Box::new(pending),
        workspace,
        prompt,
        verbose: args.verbose,
    })
}

/// Mounts a harness and runs one turn, synchronously.
///
/// Prints the answer on stdout and nothing else, persists the session, and tears the
/// composition down. Every step blocks rather than awaits, which is what keeps the
/// kernel's `block_on` from nesting inside a runtime.
fn run_turn(
    pending: compose::Pending,
    workspace: &std::path::Path,
    prompt: &str,
    verbose: bool,
) -> Result<(), String> {
    let harness = pending.start().map_err(|error| error.to_string())?;
    let mut session = harness.new_session(workspace);
    let mut reporter = StderrProgress::new(verbose, verbose);

    let outcome = kernel_block_on(harness.runner.run_turn(&mut session, prompt, &mut reporter))
        .map_err(|error| error.to_string())?;

    // The session is persisted before the answer is printed: a caller that redirects
    // stdout and loses the process should still find the transcript.
    kernel_block_on(record(&harness, &session))?;
    finish_harness(&harness)?;

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
    println!("api key: {key}");
    Ok(())
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
/// No harness is composed: reading a transcript needs no model, so the command works
/// without a configured key.
async fn prepare_list() -> Result<Ready, String> {
    let store = open_store().await.map_err(|error| error.to_string())?;
    Ok(Ready::List { store })
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
        println!(
            "{}  {} events  {}  {title}",
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
        let Some(Command::Run { task }) = args.command else {
            panic!("expected a run command");
        };
        assert_eq!(task, vec!["summarize", "this", "repo"]);
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
    fn no_subcommand_defaults_to_showing_the_configuration() {
        let args = Args::try_parse_from(["nanus"]);
        assert!(args.is_ok());
        let Ok(args) = args else {
            return;
        };
        assert!(args.command.is_none());
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
}
