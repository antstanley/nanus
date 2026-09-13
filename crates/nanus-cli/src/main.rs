//! The `nanus` binary.
//!
//! Three modes, one program:
//!
//! ```text
//! nanus run <task>     one prompt, its answer on stdout, exit
//! nanus tui            an agent for this shell, and the interface that talks to it
//! nanus service        an agent that outlives the shell, reached on a local socket
//! ```
//!
//! ## What this program is
//!
//! The core. It hosts an agent and it does not draw: for the interactive mode it runs the
//! interface as a separate program and serves it over a local socket, which is why
//! `nanus-tui` is a different process with a different dependency set. The core is the
//! thing a script invokes and the thing a boot script starts, so it is the thing that
//! must stay small enough to audit.
//!
//! ## The contract it keeps with a caller
//!
//! - **stdout carries the answer and nothing else.** Not the reasoning, not the tool
//!   calls, not a progress line. A harness whose output is a program's input cannot afford
//!   decoration on the same stream, so everything else goes to stderr.
//! - **The exit code says whether the command succeeded.** Zero only for a completed
//!   turn, a clean interface exit, or a service that started; a failed run, an exhausted
//!   step budget, or a service that is not running are non-zero, so a script can tell
//!   without parsing.
//! - **Reasoning is streamed to stderr while it happens.** A model that thinks for a
//!   minute in silence looks broken, and a user deserves to see progress without that
//!   progress contaminating the answer.
//!
//! Piped or redirected, a bare `nanus` prints its usage rather than trying to draw on
//! something that is not a terminal.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// The workspace denies `print_stdout`/`print_stderr` because a *library* that writes
// to a stream is deciding something its caller owns. This crate is the binary: its
// entire contract is what it writes to stdout (the answer) and stderr (everything
// else), so the lint is the thing that does not apply here.
#![allow(clippy::print_stdout, clippy::print_stderr)]
// The binary's modules are internal to it, so `unreachable_pub` has nothing to say.
#![allow(unreachable_pub)]

mod cli;
mod progress;
mod service;
mod tui;

use core::future::Future;
use std::process::ExitCode;

fn main() -> ExitCode {
    // The kernel is single-threaded by design, so the runtime is current-thread.
    // Installing it here means every later `block_on` — the kernel's effect reverts,
    // the adapters' I/O — shares one reactor.
    nanus_kernel::runtime::install();
    init_logging();
    // The two phases are deliberately separate calls. `prepare` awaits, which is only
    // legal inside the runtime; `finish` blocks, which is only legal outside it. Doing
    // both from one `block_on` is what produced "cannot start a runtime from within a
    // runtime".
    let outcome = nanus_kernel::runtime::block_on(cli::prepare()).and_then(cli::finish);
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The message goes to stderr so stdout stays the answer alone.
            eprintln!("nanus: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Installs a diagnostics subscriber on stderr.
///
/// `RUST_LOG` when it is set, warnings otherwise. This matters most for the service: a
/// detached process has no terminal, so its stderr is a log file and this is what makes
/// that file worth reading. Every other mode is deliberately quiet by default.
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    // `try_init` rather than `init`: installing a subscriber twice is not a reason to
    // abort a command that is otherwise fine.
    let _installed = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Drives a future to completion on the kernel runtime, with a local task set entered.
///
/// The interactive and service modes both serve a `!Send` local task — the kernel holds
/// `Rc`-shared state, so `tokio::spawn` cannot carry it — and this is the one place that
/// knows how to drive one.
pub(crate) fn block_on_local<F: Future>(future: F) -> F::Output {
    nanus_kernel::runtime::block_on_local(future)
}
