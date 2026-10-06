//! The hand-off between the runner that reserves archive capture and the `bash` tool that uses it.
//!
//! A registered tool's executor is `'static` and sees only its
//! [`ToolCall`](nanus_domain::ToolCall), so it cannot be handed a per-call reservation as an
//! argument. The runner holds the archive
//! store and the session; the tool holds the shell. The [`CaptureBroker`] is the one thing both
//! can see: the runner reserves a [`CaptureLease`] for a call *before* dispatching it and files it
//! here under the call's id; the tool takes the lease for the call it is running, archives the
//! command's streams through it, and files what each sink finalized to under the same id; the
//! runner collects those finalizations and publishes their receipts.
//!
//! Capture is opt-in and best effort. A call with no lease here runs exactly as it always did,
//! and a call whose shell cannot capture runs without an archive and files nothing — the runner
//! then reports the archive unavailable for that call, rather than the tool inventing a receipt.
//!
//! The broker is shared by `Rc` and borrowed only for the length of one method, never across an
//! `await`, so the runner and a tool running concurrently in the same local set cannot collide
//! on its cell.
//!
//! One agent runs turns in several sessions at once, and two of them can be handed the same
//! provider call id. So every entry is keyed by the session too, read from a task-local scope
//! the runner sets around a session's dispatch ([`scoped`], [`in_scope`]): a `bash` call can only
//! ever take a lease its own session reserved, and file what it finalized where its own session
//! will collect it. Outside any scope — a host that files leases itself — the key is empty.

use core::cell::RefCell;
use core::future::Future;
use std::collections::BTreeMap;
use std::rc::Rc;

use nanus_domain::ToolCallId;
use nanus_ports::{CaptureFinalization, CaptureLease};

tokio::task_local! {
    /// The session whose dispatch is running, as the broker keys it.
    static SESSION: String;
}

/// Runs `future` as the dispatch of `session`, so the broker keys what it files by that session.
pub async fn scoped<F: Future>(session: &str, future: F) -> F::Output {
    SESSION.scope(session.to_owned(), future).await
}

/// Runs `work` as part of `session`'s dispatch, synchronously.
pub fn in_scope<T>(session: &str, work: impl FnOnce() -> T) -> T {
    SESSION.sync_scope(session.to_owned(), work)
}

/// A call, qualified by the session it belongs to.
type Key = (String, ToolCallId);

/// The key of `call` in the current scope.
fn key(call: &ToolCallId) -> Key {
    let session = SESSION.try_with(Clone::clone).unwrap_or_default();
    (session, call.clone())
}

/// What the broker holds for one call.
#[derive(Default)]
struct Slot {
    /// The reservation, until the tool takes it.
    lease: Option<CaptureLease>,
    /// What the call's sinks finalized to, until the runner takes it.
    finalizations: Vec<CaptureFinalization>,
}

/// A cheap, clonable hand-off of capture leases and finalizations, keyed by tool call.
///
/// Every clone sees the same state. Each call's entry is independent: a lease filed for one call
/// is never taken by another, and finalizations are returned only to the call they were filed
/// under.
#[derive(Clone, Default)]
pub struct CaptureBroker {
    slots: Rc<RefCell<BTreeMap<Key, Slot>>>,
}

impl core::fmt::Debug for CaptureBroker {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CaptureBroker")
            .field("calls", &self.slots.borrow().len())
            .finish()
    }
}

impl CaptureBroker {
    /// Creates an empty broker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Files a lease under the call it was reserved for, returning any lease it replaces.
    ///
    /// A replaced lease is handed back rather than dropped so the caller decides when its unused
    /// allowance is released; filing two leases for one call is a runner defect, and the return
    /// value is how it notices.
    pub fn insert(&self, lease: CaptureLease) -> Option<CaptureLease> {
        let call = lease.call_id().clone();
        let mut slots = self.slots.borrow_mut();
        let slot = slots.entry(key(&call)).or_default();
        let replaced = slot.lease.replace(lease);
        assert!(
            slot.lease
                .as_ref()
                .is_some_and(|held| held.call_id() == &call),
            "a lease is filed under its own call"
        );
        replaced
    }

    /// Takes the lease filed for `call`, once.
    pub fn take_lease(&self, call: &ToolCallId) -> Option<CaptureLease> {
        let key = key(call);
        let mut slots = self.slots.borrow_mut();
        let slot = slots.get_mut(&key)?;
        let lease = slot.lease.take();
        if slot.finalizations.is_empty() {
            slots.remove(&key);
        }
        lease
    }

    /// Returns whether a lease is waiting for `call`.
    #[must_use]
    pub fn has_lease(&self, call: &ToolCallId) -> bool {
        self.slots
            .borrow()
            .get(&key(call))
            .is_some_and(|slot| slot.lease.is_some())
    }

    /// Files what `call`'s sinks finalized to, after any already filed for it.
    pub fn record(&self, call: &ToolCallId, finalizations: Vec<CaptureFinalization>) {
        if finalizations.is_empty() {
            return;
        }
        let mut slots = self.slots.borrow_mut();
        let slot = slots.entry(key(call)).or_default();
        slot.finalizations.extend(finalizations);
        assert!(
            !slot.finalizations.is_empty(),
            "a recorded call holds what it recorded"
        );
    }

    /// Takes every finalization filed for `call`, leaving none behind.
    pub fn take_finalizations(&self, call: &ToolCallId) -> Vec<CaptureFinalization> {
        let key = key(call);
        let mut slots = self.slots.borrow_mut();
        let Some(slot) = slots.get_mut(&key) else {
            return Vec::new();
        };
        let taken = core::mem::take(&mut slot.finalizations);
        if slot.lease.is_none() {
            slots.remove(&key);
        }
        taken
    }

    /// Forgets everything filed for `call`: an untaken lease is dropped, releasing its unused
    /// allowance, and untaken finalizations are discarded unpublished.
    pub fn discard(&self, call: &ToolCallId) {
        // The slot leaves the map before it is dropped, so a lease's release callback runs with
        // the cell already unborrowed.
        let key = key(call);
        let removed = self.slots.borrow_mut().remove(&key);
        drop(removed);
        assert!(
            !self.slots.borrow().contains_key(&key),
            "a discarded call is gone"
        );
    }

    /// Returns how many calls have something filed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.borrow().len()
    }

    /// Returns `true` when nothing is filed for any call.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.borrow().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;
    use nanus_domain::context::managed::{
        ArtifactReceipt, CaptureReason, CaptureStatus, CaptureStream, RawEncoding,
    };
    use nanus_ports::{CaptureFailure, RawCaptureSink, SendBoxFuture};

    /// A sink that is never written to; the broker only moves it around.
    struct Idle;

    impl RawCaptureSink for Idle {
        fn write<'a>(&'a mut self, _: &'a [u8]) -> SendBoxFuture<'a, Result<(), CaptureFailure>> {
            Box::pin(async { Ok(()) })
        }

        fn finalize(
            self: Box<Self>,
            _: u64,
            _: CaptureReason,
        ) -> SendBoxFuture<'static, CaptureFinalization> {
            Box::pin(async { unavailable("idle", CaptureStream::Stdout) })
        }
    }

    /// An unavailable finalization for `call`'s `stream`.
    fn unavailable(call: &str, stream: CaptureStream) -> CaptureFinalization {
        CaptureFinalization {
            receipt: ArtifactReceipt {
                artifact_id: None,
                call_id: call.to_owned(),
                stream,
                retained_bytes: 0,
                observed_bytes: 0,
                retained_sha256: None,
                status: CaptureStatus::Unavailable,
                reason: CaptureReason::Unsupported,
                encoding: RawEncoding::Raw,
                chunk_sha256: Vec::new(),
            },
            artifact: None,
        }
    }

    /// A lease for `call` whose release sets `released`.
    fn lease(call: &str, released: &Rc<Cell<bool>>) -> CaptureLease {
        let flag = Rc::clone(released);
        CaptureLease::new(
            ToolCallId::new(call),
            Box::new(Idle),
            Box::new(Idle),
            Box::new(move || flag.set(true)),
        )
    }

    /// Two sessions handed the same provider call id keep separate leases: each `bash` takes
    /// only the one its own session reserved, so no archive is written into another session.
    #[test]
    fn the_same_call_id_in_two_sessions_never_crosses() {
        let broker = CaptureBroker::new();
        let first = Rc::new(Cell::new(false));
        let second = Rc::new(Cell::new(false));
        assert!(in_scope("a", || broker.insert(lease("call_0", &first))).is_none());
        assert!(
            in_scope("b", || broker.insert(lease("call_0", &second))).is_none(),
            "the other session's lease is not replaced"
        );
        let id = ToolCallId::new("call_0");
        assert!(
            broker.take_lease(&id).is_none(),
            "outside both sessions there is none"
        );
        let taken = in_scope("b", || broker.take_lease(&id));
        drop(taken);
        assert!(
            second.get() && !first.get(),
            "b took b's lease, and only b's"
        );
        assert!(in_scope("a", || broker.has_lease(&id)));
        let scoped_take =
            futures::executor::block_on(scoped("a", async { broker.take_lease(&id) }));
        assert!(scoped_take.is_some(), "an async scope reads the same key");
    }

    #[test]
    fn a_lease_is_taken_once_and_only_by_its_own_call() {
        let broker = CaptureBroker::new();
        let released = Rc::new(Cell::new(false));
        assert!(broker.insert(lease("c1", &released)).is_none());
        let shared = broker.clone();
        assert!(
            shared.has_lease(&ToolCallId::new("c1")),
            "clones share the state"
        );
        assert!(
            broker.take_lease(&ToolCallId::new("c2")).is_none(),
            "not another call's"
        );
        let taken = broker.take_lease(&ToolCallId::new("c1"));
        assert!(taken.is_some());
        assert!(
            broker.take_lease(&ToolCallId::new("c1")).is_none(),
            "only once"
        );
        assert!(broker.is_empty());
        assert!(!released.get(), "taking a lease does not release it");
        drop(taken);
        assert!(released.get());
    }

    #[test]
    fn a_second_lease_for_a_call_is_handed_back_rather_than_lost() {
        let broker = CaptureBroker::new();
        let (first, second) = (Rc::new(Cell::new(false)), Rc::new(Cell::new(false)));
        assert!(broker.insert(lease("c1", &first)).is_none());
        let replaced = broker.insert(lease("c1", &second));
        assert!(replaced.is_some(), "the replaced lease comes back");
        assert!(!first.get(), "and is not released behind the caller's back");
        drop(replaced);
        assert!(first.get() && !second.get());
    }

    #[test]
    fn finalizations_are_returned_to_their_own_call_once() {
        let broker = CaptureBroker::new();
        let c1 = ToolCallId::new("c1");
        broker.record(&c1, vec![unavailable("c1", CaptureStream::Stdout)]);
        broker.record(&c1, vec![unavailable("c1", CaptureStream::Stderr)]);
        assert!(broker.take_finalizations(&ToolCallId::new("c2")).is_empty());
        let taken = broker.take_finalizations(&c1);
        let streams: Vec<_> = taken.iter().map(|done| done.receipt.stream).collect();
        assert_eq!(streams, vec![CaptureStream::Stdout, CaptureStream::Stderr]);
        assert!(broker.take_finalizations(&c1).is_empty(), "only once");
        assert!(broker.is_empty());
        // Recording nothing files nothing.
        broker.record(&c1, Vec::new());
        assert!(broker.is_empty());
    }

    #[test]
    fn discarding_a_call_releases_its_lease_and_drops_its_finalizations() {
        let broker = CaptureBroker::new();
        let released = Rc::new(Cell::new(false));
        broker.insert(lease("c1", &released));
        broker.record(
            &ToolCallId::new("c1"),
            vec![unavailable("c1", CaptureStream::Stdout)],
        );
        broker.insert(lease("c2", &Rc::new(Cell::new(false))));
        assert_eq!(broker.len(), 2);
        broker.discard(&ToolCallId::new("c1"));
        assert!(released.get(), "the unused allowance is released");
        assert!(broker.take_finalizations(&ToolCallId::new("c1")).is_empty());
        assert!(
            broker.has_lease(&ToolCallId::new("c2")),
            "another call is untouched"
        );
        assert_eq!(broker.len(), 1);
    }
}
