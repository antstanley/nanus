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
//! A binary calls [`shutdown`] after its compositions have stopped, before process exit.

use core::future::Future;
use std::cell::RefCell;

use tokio::runtime::{Builder, Runtime};
use tokio::task::LocalSet;

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

/// Drives `future` to completion on this thread's kernel runtime, with a local task
/// set entered.
///
/// Use this when `future` spawns work with [`tokio::task::spawn_local`]. Kernel futures
/// hold `Rc`-shared state and are therefore not `Send`, so they cannot go through
/// [`tokio::spawn`]; `spawn_local` is the alternative, and it is both *legal* and
/// *driven* only inside a [`LocalSet`]. Entering one is not enough on its own — the
/// runtime has to be running for the spawned task to be polled at all, which is why
/// this is a `block_on` rather than an `enter`.
///
/// Prefer [`block_on`] when nothing is spawned: a local set exists to run local tasks,
/// and one that has none is pure overhead.
///
/// # Panics
///
/// Panics when the runtime cannot be constructed, as [`block_on`] does.
pub fn block_on_local<F>(future: F) -> F::Output
where
    F: Future,
{
    install();
    let local = LocalSet::new();
    RUNTIME.with(|slot| {
        let borrowed = slot.borrow();
        let Some(runtime) = borrowed.as_ref() else {
            unreachable!("install above guarantees a runtime on this thread")
        };
        local.block_on(runtime, future)
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

/// Removes this thread's runtime and waits for its blocking workers to finish.
///
/// Call after stopping every composition and dropping its handles, outside async execution.
/// Repeated calls are harmless. A later [`install`] or [`block_on`] creates a new runtime.
/// Explicit shutdown matters on Windows: process-exit TLS destruction can run after other
/// threads have been forcibly terminated, so a worker pool must not be left to that destructor.
///
/// # Panics
///
/// Panics if called inside a Tokio runtime. Runtime shutdown must be synchronous, like mounting
/// and unmounting a composition.
pub fn shutdown() {
    assert!(
        tokio::runtime::Handle::try_current().is_err(),
        "runtime shutdown must happen outside async execution"
    );
    // Release the TLS borrow before dropping the runtime: its task destructors may consult TLS.
    let runtime = RUNTIME.with(|slot| slot.borrow_mut().take());
    drop(runtime);
    assert!(!is_installed(), "shutdown removes the thread's runtime");
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
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    #[test]
    fn block_on_completes_a_future() {
        let value = block_on(async { 7_u32 });
        assert_eq!(value, 7);
    }

    #[test]
    fn block_on_local_drives_a_spawned_local_task_to_completion() {
        // The interactive interface submits a prompt by spawning a `!Send` turn with
        // `spawn_local`. That panics outside a `LocalSet`, and a `LocalSet` that is only
        // *entered* never polls the task it spawned — so both halves matter: the spawn
        // must be legal, and the task must actually run.
        let ran = Rc::new(Cell::new(false));
        let flag = Rc::clone(&ran);
        let value = block_on_local(async move {
            tokio::task::spawn_local(async move {
                flag.set(true);
            })
            .await
            .unwrap_or_else(|error| panic!("the local task failed: {error}"));
            7_u32
        });
        assert_eq!(value, 7);
        assert!(ran.get(), "the spawned task must have been driven");
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

    #[test]
    fn runtime_shutdown_joins_workers_and_allows_a_fresh_runtime() {
        let value = block_on(async { tokio::task::spawn_blocking(|| 42).await.unwrap() });
        assert_eq!(value, 42);
        assert!(is_installed());
        shutdown();
        assert!(!is_installed());
        shutdown();
        assert!(!is_installed());
        assert_eq!(block_on(async { 43 }), 43);
        assert!(is_installed());
        shutdown();
    }

    #[test]
    fn runtime_shutdown_inside_async_execution_is_refused_without_removing_it() {
        block_on(async {
            assert!(std::panic::catch_unwind(shutdown).is_err());
            assert!(is_installed());
        });
        assert!(is_installed());
        assert_eq!(block_on(async { 44 }), 44);
        shutdown();
    }
}
