//! # nanus-adapter-store
//!
//! The session store: durable, atomic, append-only session logs under a nanus
//! home, implementing [`nanus_ports::StorePort`].
//!
//! ## Layout and home resolution
//!
//! ```text
//! <home>/sessions/<encoded-session-id>/session.jsonl
//! ```
//!
//! | Precedence | Source |
//! |---|---|
//! | 1 | an explicit path passed to [`JsonlStore::new`] |
//! | 2 | `$NANUS_HOME` |
//! | 3 | `<config dir>/nanus` via [`etcetera`] |
//!
//! ## The three guarantees
//!
//! - **Atomic writes.** A save writes a sibling temp file, fsyncs it, and renames
//!   it, so no reader ever sees a partially written session under the real name.
//! - **Trustworthy reads.** A load refuses a truncated tail, a bad header, a
//!   header from a newer build, and a hole in the event numbering. The port's
//!   [`nanus_ports::StoreError::Corrupt`] is the typed error; the message names
//!   which of the four happened, because the port has one variant for all of them.
//! - **Cheap identity.** A listing parses each header strictly and the body only
//!   leniently, so a damaged body still lists with correct id, time, and
//!   directory, and one unreadable session never hides the others.
//!
//! ## Publishing the adapter
//!
//! ```no_run
//! use nanus_adapter_store::JsonlStore;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let store = JsonlStore::from_env().await?;
//! let handle = store.handle(); // Rc<Box<dyn StorePort>> for `store_key()`
//! # let _ = handle;
//! # Ok(())
//! # }
//! ```
//!
//! ## Session ids
//!
//! Ids are UUID v7 ([`new_session_id`]), which are time-ordered, so a directory
//! listing sorts by creation. They are encoded into directory names so that an id
//! can never traverse: only `[A-Za-z0-9._-]` stays literal and everything else is
//! percent-hex-escaped.

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

mod store;

pub use store::{HOME_ENV, JsonlStore, new_session_id, resolve_home};
