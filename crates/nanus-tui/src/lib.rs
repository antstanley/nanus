//! # nanus-tui
//!
//! The interactive terminal interface: a chat view over a running agent session,
//! drawn with [`ratatui`].
//!
//! ## Why the view is separable from the runtime
//!
//! The interface is a function of a [`Transcript`] — an ordered list of entries
//! with a streaming tail — and of an [`InputBuffer`]. Neither knows what an agent
//! is. That is what lets the whole interface, including its scrolling, wrapping,
//! and key handling, be tested headlessly against ratatui's `TestBackend`, which is
//! both faster and far more precise than driving a real terminal.
//!
//! The runtime half lives behind the `runtime` feature: it owns the terminal,
//! runs the agent, and feeds the view.
//!
//! ## What the interface shows
//!
//! - **The conversation**, with the model's reasoning rendered distinctly from its
//!   answer, because the two are different things and a reader needs to tell them
//!   apart at a glance.
//! - **Tool calls and their results**, so a reader can see what the agent *did*, not
//!   only what it said.
//! - **One line for the machinery, by default**: a tool call is a single line naming the
//!   tool and what it is acting on, and a thinking segment is the newest line of itself.
//!   See [`compact`], and [`Detail::Full`] for the whole of both.
//! - **Live status**: whether a turn is open, which step it is on, and the token
//!   usage of the session so far.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// A `pub` item inside a private module is reachable only through this crate's own
// re-exports. The lint cannot tell that from an orphaned item, and every such item
// here is deliberate.
#![allow(unreachable_pub)]

pub mod buffer;
pub mod command;
pub mod compact;
pub mod notice;
pub mod replay;
pub mod stats;
pub mod transcript;
pub mod view;

#[cfg(feature = "runtime")]
pub mod runtime;

pub use buffer::{InputBuffer, KeyOutcome};
pub use command::{Command, Submission, submission_of};
pub use compact::Detail;
pub use notice::{Ending, stopping};
pub use replay::{recording_of, transcript_of};
pub use stats::{Generation, Throughput};
pub use transcript::{Entry, EntryKind, Role, Transcript};
pub use view::{Theme, ViewState};
