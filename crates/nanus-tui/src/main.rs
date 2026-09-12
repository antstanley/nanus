//! The `nanus-tui` binary: an interactive conversation with a composed harness.
//!
//! The binary is behind the `runtime` feature because without a harness there is
//! nothing to talk to. It composes the same plugin set the headless entry point does —
//! the interface is a view over the harness, not a second implementation of it.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// The binary's modules are internal to it, so `unreachable_pub` has nothing to say.
#![allow(unreachable_pub)]
// The binary reports a startup failure to stderr, which is the only place a terminal
// application can put it before the interface exists.
#![allow(clippy::print_stderr)]

#[cfg(feature = "runtime")]
mod run {
    use nanus_adapter_config::NanusConfig;
    use nanus_bundle::compose;

    use nanus_bundle::compose::Pending;

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
}

#[cfg(feature = "runtime")]
fn main() -> std::process::ExitCode {
    nanus_kernel::runtime::install();
    // Await the adapters, then leave the runtime before mounting.
    let bootstrapped = nanus_kernel::runtime::block_on(run::bootstrap());
    let outcome = bootstrapped.and_then(run::mount);
    match outcome {
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
