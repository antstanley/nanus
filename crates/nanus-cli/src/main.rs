//! The `nanus` binary.
//!
//! One entry point, three modes, and a deliberate agreement with the reference
//! harness about what a *headless* run means:
//!
//! - **stdout carries the answer and nothing else.** Not the reasoning, not the
//!   tool calls, not a progress line. A harness whose output is a program's input
//!   cannot afford decoration on the same stream, so everything else goes to
//!   stderr.
//! - **The exit code says whether the run finished.** Zero only for a completed
//!   turn; a run that hit its step budget or failed exits non-zero, so a script can
//!   tell without parsing.
//! - **Reasoning is streamed to stderr while it happens.** A model that thinks for
//!   a minute in silence looks broken, and a user deserves to see progress without
//!   that progress contaminating the answer.

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

use std::process::ExitCode;

fn main() -> ExitCode {
    // The kernel is single-threaded by design, so the runtime is current-thread.
    // Installing it here means every later `block_on` — the kernel's effect reverts,
    // the adapters' I/O — shares one reactor.
    nanus_kernel::runtime::install();
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
