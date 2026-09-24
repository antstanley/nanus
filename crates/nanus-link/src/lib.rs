//! # nanus-link
//!
//! The local link between an interface and an agent: the wire protocol, the socket
//! transport, and the client that speaks them.
//!
//! ## Why this exists
//!
//! The interface used to be a library the `nanus` binary linked, which kept the two
//! halves honest about configuration and workspace but put ratatui, the event loop, and
//! the whole view layer inside the core binary's address space. The interface is now its
//! own program, and a program cannot call a function in another program — so the seam
//! that used to be a function call is a socket.
//!
//! ## Why a Unix domain socket, and what "in-memory" means here
//!
//! There is no safe in-process channel between two processes: sharing memory across a
//! `fork` needs `mmap` and `unsafe`, and this workspace forbids `unsafe` everywhere. A
//! Unix domain socket is the local equivalent — the kernel copies bytes between two file
//! descriptors and no packet ever reaches a network interface. There is no port, no
//! listener on an address, and the socket lives in the user's own nanus home with its
//! permissions narrowed to that user, so the reachable set is "processes already running
//! as you", which can read the workspace and the session store anyway.
//!
//! The transport is a Unix socket rather than a named pipe because the workspace already
//! depends on `nix` for process groups, so Windows was never a target.
//!
//! ## The two halves
//!
//! - [`protocol`] — the vocabulary, and nothing else. Both halves depend on it and on
//!   nothing else of each other's.
//! - [`client`] — what an interface uses. Depends on no agent: the TUI binary links this
//!   half and therefore links neither the agent loop nor any adapter.
//! - [`server`] (behind the `server` feature) — what the `nanus` binary uses to serve an
//!   agent it owns.
//!
//! ## A worked exchange
//!
//! ```
//! # use nanus_link::{Frame, Request};
//! let prompt = Request::Prompt { text: "summarise this repository".to_owned() };
//! let line = nanus_link::protocol::encode(&prompt).expect("a request encodes");
//! // One frame per line: the payload never contains a raw newline, because JSON escapes
//! // it, so the framing cannot be confused with the text.
//! assert!(!line.contains('\n'));
//! let text = Frame::Text { delta: "a\nb".to_owned() };
//! assert!(!nanus_link::protocol::encode(&text).expect("a frame encodes").contains('\n'));
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod client;
pub mod error;
pub mod paths;
pub mod protocol;
#[cfg(feature = "server")]
pub mod server;
pub mod wire;

pub use client::Client;
pub use error::{LinkError, LinkResult};
pub use protocol::{AgentInfo, Frame, GoalAction, GoalInfo, GoalState, Request, decode, encode};
#[cfg(feature = "server")]
pub use server::{Agent, Parts, bind, serve};
