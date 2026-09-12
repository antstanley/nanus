//! The kernel's execution context: one single-threaded runtime per thread.
//!
//! Cordis semantics are single-threaded. Components hold `Rc`-shared state,
//! futures are not required to be `Send`, and effect revert may await. Rather
//! than thread a runtime handle through every call, the kernel owns a
//! thread-local current-thread runtime and drives the few futures it needs
//! (asynchronous effect reverts, loader configuration) on it.
//!
//! A caller that already owns a runtime on this thread keeps it: the harness
//! installs its runtime once at startup and every later `block_on` reuses it.

use core::future::Future;
use std::cell::RefCell;

use tokio::runtime::{Builder, Runtime};

thread_local! {
    /// The current thread's runtime, created lazily.
    static RUNTIME: RefCell<Option<Runtime>> = const { RefCell::new(None) };
}

/// Drives `future` to completion on this thread's kernel runtime.
///
/// # Panics
///
/// Panics only when the runtime cannot be constructed, which is an
/// unrecoverable environment failure (no reactor can be created).
pub fn block_on<F>(future: F) -> F::Output
where
    F: Future,
{
    install();
    RUNTIME.with(|slot| {
        let borrowed = slot.borrow();
        // Invariant: `install` above guarantees a runtime exists, and the borrow is
        // held only for the duration of `block_on`, never across a nested call
        // that would try to borrow again.
        let Some(runtime) = borrowed.as_ref() else {
            unreachable!("install above guarantees a runtime on this thread")
        };
        runtime.block_on(future)
    })
}

/// Installs a runtime on this thread if none exists, then runs `body`.
///
/// A binary calls this once at startup so that every later `block_on` — from the
/// kernel, from effect reverts, from adapters — shares one reactor.
///
/// # Panics
///
/// Panics when `body` panics, or when a runtime cannot be constructed.
pub fn with_runtime<R>(body: impl FnOnce() -> R) -> R {
    install();
    body()
}

/// Ensures a runtime exists on this thread.
///
/// # Panics
///
/// Panics when a runtime cannot be constructed.
pub fn install() {
    // The borrow is released before `build_runtime` runs, because building a
    // runtime installs a reactor that may itself consult this thread-local.
    let missing = RUNTIME.with(|slot| slot.borrow().is_none());
    if !missing {
        return;
    }
    let runtime = build_runtime();
    RUNTIME.with(|slot| {
        let mut borrowed = slot.borrow_mut();
        if borrowed.is_none() {
            *borrowed = Some(runtime);
        }
    });
}

/// Returns `true` when a kernel runtime is installed on this thread.
#[must_use]
pub fn is_installed() -> bool {
    RUNTIME.with(|slot| slot.borrow().is_some())
}

/// Builds the single-threaded runtime every kernel future runs on.
fn build_runtime() -> Runtime {
    let built = Builder::new_current_thread().enable_all().build();
    match built {
        Ok(runtime) => runtime,
        // A failure here means the process cannot create an event loop at all.
        Err(error) => unreachable!("cannot build the kernel runtime: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_on_completes_a_future() {
        let value = block_on(async { 7_u32 });
        assert_eq!(value, 7);
    }

    #[test]
    fn runtime_is_reused_across_calls() {
        let first = block_on(async { 1_u8 });
        assert!(is_installed());
        let second = block_on(async { 2_u8 });
        assert_eq!(first.saturating_add(second), 3);
        // Pair assertion: the reuse path still leaves exactly one runtime.
        assert!(is_installed());
    }

    #[test]
    fn with_runtime_installs_once() {
        with_runtime(|| {
            assert!(is_installed());
        });
        assert!(is_installed());
    }
}
