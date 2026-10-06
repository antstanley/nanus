//! The stock host bindings a managed turn borrows: a store checkpoint and a session context.
//!
//! A runner is reusable across sessions, so it never captures one session's store claim. The
//! host builds these two per held session — after it has taken the writer claim — and lends
//! them to each turn through a [`nanus_ports::TurnRuntime`]. They are the whole of what the
//! runner can reach: the store through one bound session id, and the archive that store keeps.

use core::cell::{Cell, RefCell};

use nanus_domain::SessionId;
use nanus_domain::context::managed::{CheckpointReceipt, Digest, Reminder};
use nanus_ports::{
    ArtifactStore, CheckpointError, CheckpointView, ContextRuntime, ExpectedCheckpoint,
    LocalBoxFuture, Reconciled, SessionCheckpoint, StoreHandle,
};

use crate::BundleError;

/// A [`SessionCheckpoint`] bound to one session of a store, under the host's claim.
///
/// It remembers the stored identity the next commit must replace, and advances it only on an
/// acknowledged commit or a reconciliation that found the candidate on disk.
pub struct StoreCheckpoint {
    store: StoreHandle,
    id: SessionId,
    expected: RefCell<ExpectedCheckpoint>,
}

impl core::fmt::Debug for StoreCheckpoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StoreCheckpoint")
            .field("id", &self.id)
            .field("expected", &self.expected.borrow())
            .finish_non_exhaustive()
    }
}

impl StoreCheckpoint {
    /// Binds a session whose stored identity the caller already knows.
    #[must_use]
    pub fn new(store: StoreHandle, id: SessionId, expected: ExpectedCheckpoint) -> Self {
        Self {
            store,
            id,
            expected: RefCell::new(expected),
        }
    }

    /// Binds a session, reading its stored identity from the store.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Session`] when the store cannot report it.
    pub async fn bind(store: StoreHandle, id: SessionId) -> Result<Self, BundleError> {
        let expected = store
            .stored_identity(&id)
            .await
            .map_err(|error| BundleError::session(error.to_string()))?;
        Ok(Self::new(store, id, expected))
    }

    /// The bound session.
    #[must_use]
    pub const fn id(&self) -> &SessionId {
        &self.id
    }
}

impl SessionCheckpoint for StoreCheckpoint {
    fn expected(&self) -> ExpectedCheckpoint {
        self.expected.borrow().clone()
    }

    fn commit<'a>(
        &'a self,
        view: CheckpointView<'a>,
    ) -> LocalBoxFuture<'a, Result<CheckpointReceipt, CheckpointError>> {
        Box::pin(async move {
            if view.candidate.id() != &self.id || view.expected != &*self.expected.borrow() {
                return Err(CheckpointError::NotCommitted(
                    nanus_domain::context::managed::ErrorCode::StaleBase,
                ));
            }
            let receipt = self.store.checkpoint(view).await?;
            let next = ExpectedCheckpoint::after(&receipt, view.candidate.body_version());
            *self.expected.borrow_mut() = next;
            Ok(receipt)
        })
    }

    fn reconcile<'a>(
        &'a self,
        candidate_sha256: &'a Digest,
    ) -> LocalBoxFuture<'a, Result<Reconciled, CheckpointError>> {
        Box::pin(async move {
            let unknown = CheckpointError::CommitOutcomeUnknown(
                nanus_domain::context::managed::ErrorCode::CheckpointUnknown,
            );
            let stored = self
                .store
                .stored_identity(&self.id)
                .await
                .map_err(|_| unknown)?;
            let previous = self.expected.borrow().clone();
            let found = match &stored {
                ExpectedCheckpoint::Stored { file_sha256, .. } => Some(file_sha256),
                ExpectedCheckpoint::Absent => None,
            };
            if found == Some(candidate_sha256) {
                *self.expected.borrow_mut() = stored.clone();
                Ok(Reconciled::Installed(stored))
            } else if stored == previous {
                Ok(Reconciled::Kept)
            } else {
                Ok(Reconciled::Quarantined)
            }
        })
    }
}

/// The stock [`ContextRuntime`]: the store's archive and a process-held cursor key.
pub struct SessionContext {
    store: Option<StoreHandle>,
    key: [u8; 32],
    reminder: Cell<Reminder>,
}

impl core::fmt::Debug for SessionContext {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SessionContext")
            .field("archive", &self.store.is_some())
            .finish_non_exhaustive()
    }
}

impl SessionContext {
    /// Builds a context over a store's archive, with a fresh random cursor key.
    ///
    /// The key lives only in this process, so a cursor handed out before a restart cannot be
    /// verified after it and reports `cursor_expired` rather than resuming somewhere else.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::Config`] when the platform has no random source.
    pub fn new(store: Option<StoreHandle>) -> Result<Self, BundleError> {
        let mut key = [0_u8; 32];
        getrandom::fill(&mut key)
            .map_err(|error| BundleError::config(format!("no random source: {error}")))?;
        Ok(Self::with_key(store, key))
    }

    /// Builds a context with a caller-chosen key, for tests and embedding hosts.
    #[must_use]
    pub fn with_key(store: Option<StoreHandle>, key: [u8; 32]) -> Self {
        Self {
            store,
            key,
            reminder: Cell::new(Reminder::default()),
        }
    }
}

impl ContextRuntime for SessionContext {
    fn archive(&self) -> Option<&dyn ArtifactStore> {
        self.store.as_ref().and_then(|store| store.artifacts())
    }

    fn cursor_key(&self) -> &[u8] {
        &self.key
    }

    fn reminder(&self) -> Reminder {
        self.reminder.get()
    }

    fn set_reminder(&self, reminder: Reminder) {
        self.reminder.set(reminder);
    }
}
