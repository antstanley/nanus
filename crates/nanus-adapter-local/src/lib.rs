//! # nanus-adapter-local
//!
//! The local adapters: the three ports whose implementation is "this machine".
//!
//! | Port | Adapter | Constructor | What it owns |
//! |---|---|---|---|
//! | [`nanus_ports::FsPort`] | [`LocalFs`] | [`LocalFs::new`] | a workspace-rooted filesystem with escape rejection |
//! | [`nanus_ports::ShellPort`] | [`LocalShell`] | [`LocalShell::new`] | process execution with process-group lifecycle control |
//! | [`nanus_ports::ClockPort`] | [`SystemClock`] | [`SystemClock::new`] | clamping wall-clock milliseconds since the Unix epoch |
//!
//! Each adapter exposes [`handle`](LocalFs::handle), which boxes it as the
//! `Rc<Box<dyn Port>>` a kernel plugin publishes under
//! [`nanus_ports::fs_key`], [`nanus_ports::shell_key`], and
//! [`nanus_ports::clock_key`] respectively.
//!
//! ## Why there is no `unsafe` here
//!
//! The shell adapter must signal a whole *process group*, which raw `libc` does
//! with `killpg`. This crate reaches that through `nix`'s safe wrapper instead, so
//! the workspace's `unsafe_code = "forbid"` stays absolute rather than being
//! relaxed for one call.
//!
//! ## Design notes that are not obvious
//!
//! - **A path that escapes the workspace is a typed error, always.** `..`, an
//!   absolute path outside the root, and a symlink resolving outside are three
//!   faces of one failure and share [`nanus_ports::FsError::OutsideWorkspace`].
//! - **A non-zero exit code is a success.** It travels in
//!   [`nanus_ports::ShellOutcome::exit_code`]; the error channel is reserved for
//!   "the command never ran".
//! - **Output overflow truncates; it is not an error.** A chatty build is normal,
//!   so [`nanus_ports::Captured::truncated`] reports it and `total_bytes` still
//!   counts every byte that came through the pipe.
//! - **Killing reaches grandchildren.** Every run is its own process group, and
//!   [`LocalShell`]'s `kill_all` is what shutdown calls.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// A `pub` item inside a private module is reachable only through this crate's own
// re-exports. The lint cannot tell that from an orphaned item, and every such item
// here is deliberate.
#![allow(unreachable_pub)]
// The *total* relatives of the unwrap family cannot panic, and are how this crate
// states a fallback.
#![allow(
    clippy::unwrap_or_default,
    clippy::manual_unwrap_or,
    clippy::manual_unwrap_or_default
)]

mod clock;
mod fs;
mod shell;

pub use clock::SystemClock;
pub use fs::LocalFs;
pub use shell::{DEFAULT_TIMEOUT_MS, LocalShell, is_alive};
