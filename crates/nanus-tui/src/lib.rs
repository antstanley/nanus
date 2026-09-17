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
// The key list the overlay and `/help` draw. Private because it is a rendering detail of
// this crate: nobody outside needs to name a binding.
mod help;
// Reading an image off the clipboard and putting it where the tools can reach it. Private
// because it is the runtime's convenience: the view never sees it, and the wire has no way to
// carry the bytes.
mod paste;
// `@` mentions: which word the caret is in, what the workspace holds, and which file answers the
// query. Private because it is a detail of the composer rather than a surface of the crate.
mod mentions;
// The `!` escape. Private because it is the runtime's own shell rather than a display: nothing the
// view draws is a function of it.
mod shell;
// Putting a selection on the clipboard, which is the other direction from `paste`. Private for the
// same reason: it is a side effect the runtime owns, not something the view draws.
mod copy;
// The markdown renderer is an implementation detail of the view: it produces the same
// `Line`s every other entry is drawn from, and nothing outside this crate needs to name
// it. Keeping it private is what stops it becoming a second public rendering surface.
mod markdown;
pub mod notice;
pub mod queue;
pub mod replay;
pub mod stats;
pub mod summary;
pub mod transcript;
pub mod view;

#[cfg(feature = "runtime")]
pub mod runtime;

pub use buffer::{InputBuffer, KeyOutcome};
pub use command::{Command, Submission, submission_of};
pub use compact::Detail;
pub use notice::{Ending, stopping};
pub use queue::Queue;
pub use replay::{recording_of, transcript_of};
pub use stats::{Generation, Throughput};
pub use transcript::{Entry, EntryKind, Role, Transcript};
pub use view::{PendingApproval, QueueEdit, Theme, ViewState};
