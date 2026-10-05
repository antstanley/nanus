//! Checkpoints: a compare-and-replace of the session file under this process's claim.
//!
//! A checkpoint is the save a managed turn makes *while it runs*, so it has to say more than a
//! save does. It names the stored identity it expects to replace — the digest of the bytes on
//! disk, never of a re-encoding — and it answers in one of two very different ways when it fails:
//!
//! Refused before any write, so `NotCommitted` and the previous file intact:
//!
//! - the id is retired, this process does not hold its claim, or the session directory is a
//!   symlink: `PolicyDenied`;
//! - the stored identity is not the expected one — another writer saved in between: `StaleBase`;
//! - the candidate's managed records do not fold: the fold's own code;
//! - a record over 4 MiB or a session over 64 MiB: `StorageCapacity`; any other encoding
//!   refusal: `SourceCorrupt`;
//! - an artifact proof that is another session's, missing, or does not verify:
//!   `InvalidReference`, `SourceUnavailable` or `SourceCorrupt`;
//! - a receipt the candidate publishes for the first time with no matching proof:
//!   `InvalidReference`, because a dangling reference is never acknowledged;
//! - the temporary file cannot be written or synced: `CheckpointNotCommitted`.
//!
//! At the rename:
//!
//! - it fails and the disk, read back under the same claim, still shows the expected identity:
//!   `NotCommitted(CheckpointNotCommitted)` — intact because it was shown, not assumed;
//! - it fails and the disk shows anything else or cannot be read, or the id was retired while
//!   it was in flight: `CommitOutcomeUnknown(CheckpointUnknown)`, which only a reconciliation
//!   can settle.
//!
//! The store imposes no deadline of its own on a commit: a rename it stopped waiting for could
//! still land, which would turn every slow disk into an unknown outcome. A started commit runs to
//! its end, and its caller awaits it.
//!
//! ## Durability
//!
//! A receipt claims [`Durability::ProcessCrash`]: the file is synced and then renamed over the old
//! one, so a process crash leaves either the old file or the new one. It does not claim
//! [`Durability::PowerLoss`], which would need the parent directory synced after the rename and,
//! on macOS, `F_FULLFSYNC` rather than `fsync` — neither of which this store does for a session.

use nanus_domain::context::managed::{
    CheckpointReceipt, ContextFrontier, Digest, Durability, ErrorCode, ManagedState,
};
use nanus_domain::{Session, SessionError, SessionEvent, SessionId};
use nanus_ports::{
    ArtifactError, CheckpointError, CheckpointView, ExpectedCheckpoint, FinalizedArtifact,
    StoreError, StoreResult,
};
use tokio::fs;
use tokio::io::AsyncWriteExt as _;

use super::{
    JsonlStore, header_version, io_error, parse_header, read_bounded_bytes, refuse_symlinked_dir,
    temp_path,
};

/// A refusal that left the previous file intact.
const fn refused(code: ErrorCode) -> CheckpointError {
    CheckpointError::NotCommitted(code)
}

/// An outcome only a read of the disk can settle.
const fn unknown() -> CheckpointError {
    CheckpointError::CommitOutcomeUnknown(ErrorCode::CheckpointUnknown)
}

impl JsonlStore {
    /// Reads the identity of the stored file, from its bytes.
    pub(super) async fn stored_identity_blocking(
        &self,
        id: &SessionId,
    ) -> StoreResult<ExpectedCheckpoint> {
        let path = self.session_file(id)?;
        match read_bounded_bytes(&path).await {
            Ok(raw) => identity_of(id, &raw),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                Ok(ExpectedCheckpoint::Absent)
            }
            Err(source) => Err(io_error(&path, &source)),
        }
    }

    /// Commits a checkpoint; see the module documentation for every answer it gives.
    pub(super) async fn checkpoint_blocking(
        &self,
        view: CheckpointView<'_>,
    ) -> Result<CheckpointReceipt, CheckpointError> {
        let candidate = view.candidate;
        let id = candidate.id();
        self.admit(id).await?;
        let stored = self
            .stored_identity_blocking(id)
            .await
            .map_err(|_| refused(ErrorCode::CheckpointNotCommitted))?;
        if stored != *view.expected {
            return Err(refused(ErrorCode::StaleBase));
        }
        let revision = accepted_revision(candidate)?;
        let body = candidate
            .try_to_jsonl()
            .map_err(|error| refused(encoding_refusal(&error)))?;
        let count = u64::try_from(candidate.event_count())
            .map_err(|_| refused(ErrorCode::StorageCapacity))?;
        self.check_artifacts(candidate, &stored, view.artifacts)
            .await?;
        self.replace(id, &body, view.expected).await?;
        let receipt = CheckpointReceipt {
            frontier: ContextFrontier {
                session_id: id.as_str().to_owned(),
                event_count: count,
                prefix_sha256: Digest::of(body.as_bytes()),
                projection_revision: revision,
            },
            body_digest: candidate.body_digest(),
            durability: Durability::ProcessCrash,
        };
        // Postcondition: the frontier the receipt names is the candidate's own prefix at its full
        // count, which is what lets the host derive the next expected identity from it.
        assert!(
            candidate
                .prefix_digest(count)
                .is_ok_and(|digest| digest == receipt.frontier.prefix_sha256),
            "a checkpoint's frontier digest is the candidate's whole-file digest"
        );
        Ok(receipt)
    }

    /// Refuses a checkpoint this process may not write.
    async fn admit(&self, id: &SessionId) -> Result<(), CheckpointError> {
        let denied = refused(ErrorCode::PolicyDenied);
        if self.retired(id).unwrap_or(true) || !self.holds_claim(id) {
            return Err(denied);
        }
        let dir = self.session_dir(id).map_err(|_| denied)?;
        refuse_symlinked_dir(&dir).await.map_err(|_| denied)
    }

    /// Verifies every proof handed in, and that every newly published object has one.
    async fn check_artifacts(
        &self,
        candidate: &Session,
        stored: &ExpectedCheckpoint,
        artifacts: &[FinalizedArtifact],
    ) -> Result<(), CheckpointError> {
        for artifact in artifacts {
            if artifact.session() != candidate.id() {
                return Err(refused(ErrorCode::InvalidReference));
            }
            self.verify_artifact(artifact)
                .await
                .map_err(|error| refused(artifact_refusal(error)))?;
        }
        let from = match stored {
            ExpectedCheckpoint::Absent => 0,
            ExpectedCheckpoint::Stored { event_count, .. } => {
                usize::try_from(*event_count).unwrap_or(usize::MAX)
            }
        };
        // A receipt the candidate publishes for the first time is a reference this checkpoint
        // would acknowledge, so it must arrive with the host's proof that the object is
        // finalized: a model-shaped receipt with no object behind it is a dangling reference.
        let unproven = candidate
            .log()
            .events()
            .iter()
            .skip(from)
            .any(|event| match event {
                SessionEvent::ArtifactPublished { payload } => {
                    payload.artifact_id.is_some()
                        && !artifacts
                            .iter()
                            .any(|artifact| artifact.receipt() == payload.as_ref())
                }
                _ => false,
            });
        if unproven {
            return Err(refused(ErrorCode::InvalidReference));
        }
        Ok(())
    }

    /// Writes `body` beside the session file, syncs it, and renames it over the real name.
    async fn replace(
        &self,
        id: &SessionId,
        body: &str,
        expected: &ExpectedCheckpoint,
    ) -> Result<(), CheckpointError> {
        let not_committed = refused(ErrorCode::CheckpointNotCommitted);
        let path = self.session_file(id).map_err(|_| not_committed)?;
        let dir = self.session_dir(id).map_err(|_| not_committed)?;
        fs::create_dir_all(&dir).await.map_err(|_| not_committed)?;
        let temp = temp_path(&path);
        if let Err(error) = write_synced(&temp, body).await {
            tracing::warn!(%error, "a checkpoint's temporary file could not be written");
            discard(&temp).await;
            return Err(not_committed);
        }
        if let Err(source) = fs::rename(&temp, &path).await {
            discard(&temp).await;
            // A rename is atomic, so an error means it did not happen — but that is said only
            // when the disk agrees: the expected identity, read back under the same claim.
            let intact = self
                .stored_identity_blocking(id)
                .await
                .is_ok_and(|now| now == *expected);
            tracing::warn!(%source, intact, "a checkpoint's rename failed");
            return Err(if intact { not_committed } else { unknown() });
        }
        // The claim is this process's, so only this process can have retired the id meanwhile;
        // the file it just wrote is removed rather than left to resurrect the conversation.
        if self.retired(id).unwrap_or(true) {
            self.undo_resurrection(id);
            return Err(unknown());
        }
        Ok(())
    }
}

/// Parses the identity of stored bytes: the header's version and the event-line count.
fn identity_of(id: &SessionId, raw: &[u8]) -> StoreResult<ExpectedCheckpoint> {
    let mut header = None;
    let mut events: u64 = 0;
    for (index, line) in raw.split(|byte| *byte == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if header.is_some() {
            events = events.saturating_add(1);
            continue;
        }
        let number = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
        let text = std::str::from_utf8(line).map_err(|_| StoreError::Corrupt {
            id: id.as_str().to_owned(),
            message: format!("the header on line {number} is not UTF-8"),
        })?;
        header = Some(parse_header(text, id, number)?);
    }
    let header = header.ok_or_else(|| StoreError::Corrupt {
        id: id.as_str().to_owned(),
        message: String::from("the log has no header line"),
    })?;
    Ok(ExpectedCheckpoint::Stored {
        body_version: header_version(&header),
        file_sha256: Digest::of(raw),
        event_count: events,
    })
}

/// Returns the revision a candidate has accepted: zero for a body that is not managed.
fn accepted_revision(candidate: &Session) -> Result<u64, CheckpointError> {
    if !candidate.is_managed_body() {
        return Ok(0);
    }
    ManagedState::fold(candidate.log())
        .map(|state| state.revision())
        .map_err(refused)
}

/// Classifies an encoding refusal: a bound is capacity, anything else is a corrupt candidate.
///
/// The domain reports both through [`SessionError`]'s text-carrying variants, so the size bounds
/// are recognised by the words their checks use: "exceeds" in every one of them, and nowhere
/// else in the encoder's refusals.
fn encoding_refusal(error: &SessionError) -> ErrorCode {
    let text = match error {
        SessionError::BadHeader { reason, .. } => reason.as_str(),
        SessionError::MalformedEvent { detail, .. } => detail.as_str(),
        _ => "",
    };
    if text.contains("exceeds") {
        ErrorCode::StorageCapacity
    } else {
        ErrorCode::SourceCorrupt
    }
}

/// Maps an archive refusal onto the code a checkpoint refusal carries.
const fn artifact_refusal(error: ArtifactError) -> ErrorCode {
    match error {
        ArtifactError::Unavailable => ErrorCode::SourceUnavailable,
        ArtifactError::Corrupt => ErrorCode::SourceCorrupt,
        ArtifactError::Denied => ErrorCode::InvalidReference,
    }
}

/// Writes and syncs a fresh file.
async fn write_synced(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    let mut file = fs::File::create(path).await?;
    file.write_all(body.as_bytes()).await?;
    file.sync_all().await
}

/// Removes a temporary file, quietly: it was never under the real name.
async fn discard(path: &std::path::Path) {
    if fs::remove_file(path).await.is_ok() {
        tracing::debug!(temp = %path.display(), "discarded a checkpoint's temporary file");
    }
}
