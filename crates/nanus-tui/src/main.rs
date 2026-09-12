//! The `nanus-tui` binary: an interactive conversation, or a recording of one.
//!
//! Two modes, and the difference is whether a model is reachable:
//!
//! ```text
//! nanus-tui                     talk to a model in the current directory
//! nanus-tui --session           read the most recent recorded session
//! nanus-tui --session <id>      read a particular one
//! nanus-tui --sessions          list what is available to read
//! ```
//!
//! Reading needs no API key, because a transcript is already written down. That is not
//! only a convenience: it means the interface can be exercised, reviewed, and captured
//! without a credential.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// The binary's modules are internal to it, so `unreachable_pub` has nothing to say.
#![allow(unreachable_pub)]
// The binary reports a startup failure to stderr, which is the only place a terminal
// application can put it before the interface exists.
#![allow(clippy::print_stderr)]
// The usage text goes to stdout, which is where a program's own help belongs.
#![allow(clippy::print_stdout)]

#[cfg(feature = "runtime")]
mod run {
    use nanus_adapter_config::NanusConfig;
    use nanus_bundle::compose;
    use nanus_bundle::compose::Pending;
    use nanus_ports::StoreHandle;

    /// The usage text, printed for `--help` and for a usage error.
    const USAGE: &str = "usage: nanus-tui [--session [<id>] | --sessions]\n\n\
         With no arguments, talks to a model in the current directory.\n\
         That mode requires DEEPSEEK_API_KEY.\n\n\
         --session [<id>]   read a recorded session; needs no key\n\
         --sessions         list the recorded sessions\n\
         --scroll <rows>    open that many rows back from the end\n";

    /// What the command line asked for.
    pub enum Mode {
        /// Talk to a model.
        Live,
        /// List the recorded sessions.
        List,
        /// Read a recorded session, or the most recent one.
        View {
            /// The session to read.
            id: Option<String>,
            /// Rows to scroll back from the end.
            scroll_back: u32,
        },
    }

    /// Parses the command line.
    ///
    /// Hand-written rather than derived: there are three flags and no values beyond one
    /// optional id, and a dependency on an argument parser for that would be a lot of
    /// machinery to keep aligned with a grammar this small.
    ///
    /// # Errors
    ///
    /// Returns a message for an unknown flag, so a typo is reported rather than silently
    /// starting a live session.
    pub fn parse(args: &[String]) -> Result<Mode, String> {
        let mut mode = Mode::Live;
        let mut scroll_back = 0_u32;
        let mut index = 0_usize;
        // Index-based rather than an iterator: two flags take a following value, one of
        // them optionally, and an iterator makes "look at the next one, and consume it
        // only if it is a value" awkward to express correctly.
        while let Some(arg) = args.get(index) {
            index = index.saturating_add(1);
            match arg.as_str() {
                "--help" | "-h" => return Err(String::from(USAGE)),
                "--sessions" => mode = Mode::List,
                "--scroll" => {
                    let value = args.get(index).ok_or("--scroll needs a number of rows")?;
                    index = index.saturating_add(1);
                    scroll_back = value
                        .parse()
                        .map_err(|_| format!("--scroll needs a number, not {value:?}"))?;
                }
                "--session" => {
                    // The id is optional, and an id never begins with `--`, so a flag is
                    // left for the next iteration rather than being swallowed.
                    mode = Mode::View {
                        id: None,
                        scroll_back: 0,
                    };
                    if let Some(next) = args.get(index).filter(|next| !next.starts_with("--")) {
                        index = index.saturating_add(1);
                        mode = Mode::View {
                            id: Some(next.clone()),
                            scroll_back: 0,
                        };
                    }
                }
                other => return Err(format!("unknown argument {other:?}; try --help")),
            }
        }
        // `--scroll` is global, so it merges into whichever mode was selected. It has no
        // meaning for the other modes, which never open a transcript to scroll.
        Ok(match mode {
            Mode::View { id, .. } => Mode::View { id, scroll_back },
            other => other,
        })
    }

    /// Awaits the adapters a harness needs.
    ///
    /// Split from [`mount`] because the kernel drives its plugin hooks with `block_on`,
    /// which panics when called from inside a runtime. Awaiting this, leaving the
    /// runtime, and only then mounting is what keeps the two apart.
    ///
    /// # Errors
    ///
    /// Returns a rendered message when the configuration or the adapters are unusable.
    pub async fn bootstrap() -> Result<Pending, String> {
        let config = NanusConfig::load(None).map_err(|error| error.to_string())?;
        compose(&config).await.map_err(|error| error.to_string())
    }

    /// Mounts the harness and runs the interface.
    ///
    /// Synchronous, and deliberately so: it is the half that must not be inside a
    /// runtime.
    ///
    /// # Errors
    ///
    /// Returns a rendered message for any failure, because a terminal application has
    /// nowhere to put a structured error by the time it is running.
    pub fn mount(pending: Pending) -> Result<(), String> {
        let harness = pending.start().map_err(|error| error.to_string())?;
        nanus_tui::runtime::run(&harness).map_err(|error| error.to_string())
    }

    /// Opens the session store, which needs no model.
    ///
    /// # Errors
    ///
    /// Returns a rendered message when the store cannot be opened.
    pub async fn store() -> Result<StoreHandle, String> {
        compose::open_store()
            .await
            .map_err(|error| error.to_string())
    }

    /// Dispatches one invocation.
    ///
    /// # Errors
    ///
    /// Returns a rendered message for any failure.
    pub fn dispatch(mode: Mode) -> Result<(), String> {
        match mode {
            Mode::Live => {
                // Enter the runtime for the adapters, leave it before mounting.
                let bootstrapped = nanus_kernel::runtime::block_on(bootstrap());
                bootstrapped.and_then(mount)
            }
            Mode::List => {
                let store = nanus_kernel::runtime::block_on(store())?;
                nanus_tui::runtime::list_sessions(&store)
            }
            Mode::View { id, scroll_back } => {
                let store = nanus_kernel::runtime::block_on(store())?;
                nanus_tui::runtime::view(&store, id.as_deref(), scroll_back)
            }
        }
    }

    /// The binary's entry point.
    ///
    /// # Errors
    ///
    /// Returns a rendered message for any failure.
    pub fn main(args: &[String]) -> Result<(), String> {
        match parse(args) {
            Ok(mode) => dispatch(mode),
            // `--help` is not a failure: it is printed and the run is over.
            Err(message) => {
                if message.starts_with("usage:") {
                    print!("{message}");
                    return Ok(());
                }
                Err(message)
            }
        }
    }
}

#[cfg(feature = "runtime")]
fn main() -> std::process::ExitCode {
    nanus_kernel::runtime::install();
    // Arguments after the binary name.
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run::main(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nanus-tui: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(feature = "runtime"))]
fn main() -> std::process::ExitCode {
    // Without the feature there is no harness to drive the view, and an entry point
    // that cannot run is worse than one that says so.
    eprintln!(
        "nanus-tui: build with `--features runtime` to use the interactive interface.\n\
         The view layer is complete and tested: cargo nextest run -p nanus-tui"
    );
    std::process::ExitCode::from(2)
}
