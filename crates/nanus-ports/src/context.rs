//! Session-scoped managed-context contracts: checkpoints and the per-turn runtime.
//!
//! A managed turn saves *while it runs*: a settled prefix and a request intent before HTTP, each
//! settled step after it, and a context revision before any request uses it. That is a different
//! obligation from the terminal save an ordinary turn ends with, so it has its own port rather
//! than a second meaning for [`crate::StorePort::save`]:
//!
//! - the host binds a [`SessionCheckpoint`] to its own writer claim, store and frontier, and the
//!   runner commits through it and nothing else;
//! - a commit names the stored identity it expects to replace, so a stale or concurrent writer
//!   is refused instead of overwritten;
//! - a failure says which of two very different things happened — [`CheckpointError::NotCommitted`]
//!   (the previous file is intact) or [`CheckpointError::CommitOutcomeUnknown`] (it may or may
//!   not have been replaced) — and the runner's [`PersistenceState`] tells the host what it may
//!   do next.
//!
//! The runner never borrows a store, and a reusable runner never captures one session's claim:
//! everything session-scoped arrives per turn in a [`TurnRuntime`].

use nanus_domain::context::managed::{ArtifactReceipt, CheckpointReceipt, Digest, ErrorCode};
use nanus_domain::{Session, SessionId};

use crate::LocalBoxFuture;
use crate::artifact::ArtifactStore;

/// The stored identity a commit expects to replace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExpectedCheckpoint {
    /// Nothing is stored yet: a new session.
    Absent,
    /// The file on disk, as it actually is — never a re-encoding of it.
    Stored {
        /// The body version its header declares.
        body_version: u32,
        /// BLAKE3 of the whole stored file.
        file_blake3: Digest,
        /// How many events it holds.
        event_count: u64,
    },
}

impl ExpectedCheckpoint {
    /// The identity a successful commit leaves on disk.
    ///
    /// Derived, not stored: the receipt's frontier covers the whole file at its full count, so
    /// its digest *is* the file's digest, and the body version is the candidate's.
    #[must_use]
    pub fn after(receipt: &CheckpointReceipt, body_version: u32) -> Self {
        Self::Stored {
            body_version,
            file_blake3: receipt.frontier.prefix_blake3.clone(),
            event_count: receipt.frontier.event_count,
        }
    }
}

/// Why a checkpoint is being written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckpointReason {
    /// Managed mode was enabled and the body upgraded.
    Activate,
    /// A settled prefix and a request intent, before HTTP.
    RequestIntent,
    /// A step settled.
    SettledStep,
    /// A candidate context revision.
    ContextRevision,
    /// The turn ended.
    TurnEnd,
    /// A resumed session closed what a crash left open.
    Recovery,
    /// A person reset the session's context.
    Reset,
}

/// A host-created proof that an archived object is finalized, immutable and quiescent.
///
/// Never model-supplied: the archive creates one when a capture finalizes, and a checkpoint
/// verifies it against actual storage before it acknowledges a reference to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizedArtifact {
    session: SessionId,
    receipt: ArtifactReceipt,
}

impl FinalizedArtifact {
    /// Wraps the receipt of an object the archive has finalized for `session`.
    #[must_use]
    pub const fn new(session: SessionId, receipt: ArtifactReceipt) -> Self {
        Self { session, receipt }
    }

    /// The session the object belongs to.
    #[must_use]
    pub const fn session(&self) -> &SessionId {
        &self.session
    }

    /// The receipt.
    #[must_use]
    pub const fn receipt(&self) -> &ArtifactReceipt {
        &self.receipt
    }
}

/// One checkpoint request.
#[derive(Clone, Copy, Debug)]
pub struct CheckpointView<'a> {
    /// The complete session to make durable.
    pub candidate: &'a Session,
    /// The stored identity it replaces.
    pub expected: &'a ExpectedCheckpoint,
    /// Why.
    pub reason: CheckpointReason,
    /// Archived objects the candidate newly references.
    pub artifacts: &'a [FinalizedArtifact],
}

/// Why a checkpoint failed, and what that means for the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CheckpointError {
    /// The previous durable file is intact; nothing was replaced.
    #[error("the checkpoint was not committed ({0}); the previous file is intact")]
    NotCommitted(ErrorCode),
    /// The replacement may or may not have happened; only a read of the disk can say.
    #[error("whether the checkpoint was committed is unknown ({0})")]
    CommitOutcomeUnknown(ErrorCode),
}

/// What a reconciliation found on disk after an unknown outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reconciled {
    /// The candidate is on disk: install it.
    Installed(ExpectedCheckpoint),
    /// The previous file is on disk: keep the old projection.
    Kept,
    /// Neither: continuation is quarantined until explicit recovery.
    Quarantined,
}

/// The host's bound checkpoint: one session, one writer claim, one store.
pub trait SessionCheckpoint {
    /// The stored identity the next commit must replace.
    fn expected(&self) -> ExpectedCheckpoint;

    /// Verifies the expected identity and the candidate, then atomically replaces the session.
    ///
    /// On success the implementation has advanced [`SessionCheckpoint::expected`] to the new
    /// identity. A started commit is never raced against cancellation: its caller awaits it.
    fn commit<'a>(
        &'a self,
        view: CheckpointView<'a>,
    ) -> LocalBoxFuture<'a, Result<CheckpointReceipt, CheckpointError>>;

    /// Reads the disk under the same claim after an unknown outcome.
    ///
    /// `candidate_blake3` is the digest of the file the uncertain commit would have written.
    fn reconcile<'a>(
        &'a self,
        candidate_blake3: &'a Digest,
    ) -> LocalBoxFuture<'a, Result<Reconciled, CheckpointError>>;
}

/// Whether a managed run's session is durable, and what the host may still do about it.
///
/// Returned with every managed run, failed or not. `Acknowledged` permits no duplicate save;
/// `Unsaved` permits one reserved terminal attempt; `Unknown` permits only reconciliation —
/// overwriting an uncertain commit is exactly the transaction violation this exists to prevent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PersistenceState {
    /// The last checkpoint was acknowledged.
    Acknowledged(CheckpointReceipt),
    /// The last commit was refused and the previous file is intact.
    Unsaved {
        /// The identity still on disk.
        last: ExpectedCheckpoint,
    },
    /// The last commit's outcome is unknown.
    Unknown {
        /// The identity before it.
        previous: ExpectedCheckpoint,
        /// The digest of the file it would have written.
        candidate_blake3: Digest,
    },
}

/// What a managed turn needs from its host beyond the checkpoint.
///
/// Binds the session's archive and the process-held key recall cursors are sealed with. It never
/// lends the running session's mutable log: loop-owned context tools read the session the runner
/// already holds, at a step boundary.
pub trait ContextRuntime {
    /// The session's archive, when capture or artifact recall is available.
    fn archive(&self) -> Option<&dyn ArtifactStore>;

    /// The key recall and inspect cursors are sealed with, held only by this process.
    fn cursor_key(&self) -> &[u8];

    /// The soft-pressure reminder state, carried between turns.
    fn reminder(&self) -> nanus_domain::context::managed::Reminder;

    /// Stores the reminder state.
    fn set_reminder(&self, reminder: nanus_domain::context::managed::Reminder);
}

/// What one managed turn is lent by its host.
#[derive(Clone, Copy, Default)]
pub struct TurnRuntime<'a> {
    /// The session-scoped context runtime.
    pub context: Option<&'a dyn ContextRuntime>,
    /// The bound checkpoint.
    pub checkpoint: Option<&'a dyn SessionCheckpoint>,
}

impl core::fmt::Debug for TurnRuntime<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TurnRuntime")
            .field("context", &self.context.is_some())
            .field("checkpoint", &self.checkpoint.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanus_domain::context::managed::{ContextFrontier, Durability};

    #[test]
    fn the_identity_after_a_commit_is_the_receipts_whole_file() {
        let receipt = CheckpointReceipt {
            frontier: ContextFrontier {
                session_id: "s".into(),
                event_count: 7,
                prefix_blake3: Digest::of(b"file"),
                projection_revision: 2,
            },
            body_digest: Digest::of(b"body"),
            durability: Durability::ProcessCrash,
        };
        assert_eq!(
            ExpectedCheckpoint::after(&receipt, 3),
            ExpectedCheckpoint::Stored {
                body_version: 3,
                file_blake3: Digest::of(b"file"),
                event_count: 7,
            }
        );
    }

    #[test]
    fn the_two_failures_say_different_things() {
        let refused = CheckpointError::NotCommitted(ErrorCode::StorageCapacity).to_string();
        let unknown =
            CheckpointError::CommitOutcomeUnknown(ErrorCode::CheckpointUnknown).to_string();
        assert!(refused.contains("intact"), "{refused}");
        assert!(unknown.contains("unknown"), "{unknown}");
    }
}
