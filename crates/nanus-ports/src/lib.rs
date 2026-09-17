//! # nanus-ports
//!
//! The hexagon's boundary: the traits nanus adapters implement and the harness
//! consumes. Nothing here performs I/O. A port is a *declaration* of a
//! capability — filesystem, shell, model, store, clock — and an adapter is a
//! plugin that publishes an implementation of one under a key.
//!
//! ```
//! use nanus_ports::{fs_key, FsHandle};
//! use nanus_kernel::ServiceKey;
//!
//! // The key is the whole contract between a provider and a consumer: they
//! // agree on a name and on the type published under it, and nothing else.
//! let key: ServiceKey<FsHandle> = fs_key();
//! assert_eq!(key.as_str(), "fs");
//! ```
//!
//! ## Why handles are `Rc<Box<dyn Port>>`
//!
//! The kernel's service registry erases a published value to `dyn Any` and
//! recovers it by downcasting, and Rust can only downcast to a *sized* type.
//! `Rc<dyn FsPort>` is unsized and therefore unrecoverable; `Rc<Box<dyn FsPort>>`
//! is a sized `Rc` of a boxed trait object and works. Every port is shared that
//! way, behind a `Handle` alias, and the key functions below are the one place a
//! provider and a consumer agree on the pair.
//!
//! ## Why the futures are boxed by hand
//!
//! All four I/O ports are `async` in effect and none of them uses `async fn`.
//! `async fn` in a trait is stable, but the resulting trait is not
//! *dyn-compatible*, and these ports must be trait objects to be published by
//! key. Each method is therefore written as the desugaring `async fn` would
//! perform: an ordinary function returning [`LocalBoxFuture`].
//!
//! [`LocalBoxFuture`] is `!Send`. The kernel is single-threaded, like Cordis, so
//! requiring `Send` would demand a proof no caller needs. Note that the kernel
//! has its own `LocalBoxFuture` alias for event callbacks; this one is generic
//! over its output and carries a lifetime, which is what a port method needs.
//!
//! ## Where the tool registry lives
//!
//! There is deliberately **no `ToolPort`** in this crate. The tool registry is
//! [`nanus_domain::ToolRegistry`], because a registry is policy rather than I/O:
//! it rejects duplicate registrations, projects its contents onto the wire
//! allowlist, validates arguments before dispatch, and converts a pre-dispatch
//! failure into a model-visible [`nanus_domain::ToolOutcome::Failure`] instead
//! of a harness error. Its only outside dependency is
//! [`nanus_domain::ToolExecutor`], which an adapter implements directly —
//! typically by calling the filesystem or shell port it was given. Putting a
//! `ToolPort` here would either duplicate that policy or hide it behind a trait
//! that adds nothing.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// A `pub` item inside a private module is reachable only through this crate's own
// re-exports. The lint cannot tell that from an orphaned item, and every such item
// here is deliberate.
#![allow(unreachable_pub)]
// The *total* relatives of the unwrap family cannot panic; see the same note in
// `nanus-domain`.
#![allow(
    clippy::unwrap_or_default,
    clippy::manual_unwrap_or,
    clippy::manual_unwrap_or_default
)]

pub mod clock;
pub mod error;
pub mod fs;
pub mod llm;
pub mod shell;
pub mod store;

use core::future::Future;
use core::pin::Pin;

use nanus_kernel::ServiceKey;

pub use clock::{ClockHandle, ClockPort};
pub use error::{PortError, PortResult};
pub use fs::{
    DirEntry, EditOutcome, FileMeta, FileRead, FsError, FsHandle, FsPort, FsResult, SearchKind,
    SearchMatch, SearchOutcome, SearchQuery, WriteMode, WriteOutcome, check_edit_count,
    ensure_within, normalize, occurrence_count,
};
pub use llm::{
    ChatRequest, FinishReason, LlmError, LlmEvent, LlmHandle, LlmPort, LlmResult, LlmStream,
    ReasoningEffort, ToolCallAssembler, error_body_snippet, truncate_chars,
};
pub use nanus_domain::{ApprovalOutcome, ApprovalPolicy, SandboxMode, ToolAccess};
pub use shell::{
    Captured, DEFAULT_MAX_OUTPUT_BYTES, PLATFORM_SHELL, PLATFORM_SHELL_FLAG, SandboxPolicy,
    ShellError, ShellEvent, ShellHandle, ShellOutcome, ShellPort, ShellRequest, ShellResult,
    ShellStream,
};
pub use store::{SessionSummary, StoreError, StoreHandle, StorePort, StoreResult};

/// A boxed future that need not be `Send`.
///
/// Generic over its output and its lifetime, which is what a port method needs:
/// the future borrows the port for as long as the operation runs, and the
/// operation's result is whatever the method returns.
pub type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// The key a model adapter publishes itself under.
///
/// Constructed once by the provider and once by each consumer; the two agree
/// because the name and the type are both in this function.
#[must_use]
pub fn llm_key() -> ServiceKey<LlmHandle> {
    ServiceKey::of("llm")
}

/// The key a filesystem adapter publishes itself under.
#[must_use]
pub fn fs_key() -> ServiceKey<FsHandle> {
    ServiceKey::of("fs")
}

/// The key a shell adapter publishes itself under.
#[must_use]
pub fn shell_key() -> ServiceKey<ShellHandle> {
    ServiceKey::of("shell")
}

/// The key a session store publishes itself under.
#[must_use]
pub fn store_key() -> ServiceKey<StoreHandle> {
    ServiceKey::of("store")
}

/// The key a clock publishes itself under.
#[must_use]
pub fn clock_key() -> ServiceKey<ClockHandle> {
    ServiceKey::of("clock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanus_kernel::AnyServiceKey;

    #[test]
    fn every_key_has_a_distinct_name() {
        let names = [
            llm_key().as_str(),
            fs_key().as_str(),
            shell_key().as_str(),
            store_key().as_str(),
            clock_key().as_str(),
        ];
        assert_eq!(names, ["llm", "fs", "shell", "store", "clock"]);
        // Negative space: identity is the pair (name, type), and every port has
        // its own type, so no two keys can be confused even if a name collided.
        let erased: Vec<AnyServiceKey> = vec![
            AnyServiceKey::from_typed(llm_key()),
            AnyServiceKey::from_typed(fs_key()),
            AnyServiceKey::from_typed(shell_key()),
            AnyServiceKey::from_typed(store_key()),
            AnyServiceKey::from_typed(clock_key()),
        ];
        for (index, left) in erased.iter().enumerate() {
            for (other, right) in erased.iter().enumerate() {
                if index == other {
                    continue;
                }
                assert_ne!(left, right, "port keys are pairwise distinct");
            }
        }
    }

    #[test]
    fn a_key_is_stable_across_calls() {
        // A provider and a consumer build the key independently; the registry
        // only works if the two builds agree.
        assert_eq!(llm_key(), llm_key());
        assert_eq!(fs_key().type_id(), fs_key().type_id());
        assert_ne!(shell_key().name(), store_key().name());
        assert_ne!(store_key().name(), clock_key().name());
        assert_eq!(store_key().as_str(), "store");
    }
}
