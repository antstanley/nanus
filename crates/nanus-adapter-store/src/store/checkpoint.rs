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
//! ## What a checkpoint costs
//!
//! A managed turn checkpoints several times, so a checkpoint costs what the step *added*, not what
//! the conversation has grown to, wherever it can:
//!
//! - **The stored identity is remembered, not re-read.** After a commit this process records the
//!   identity it wrote and the file's length, modification time and (on Unix) inode. The next
//!   checkpoint compares only those; any difference — another writer, an edit, a deletion — and
//!   the file is read back and digested exactly as before. A writer that replaced the file in
//!   place, at the same length, within the clock's resolution, would go unseen; the session claim
//!   is what rules such a writer out, and this is a check behind it rather than instead of it.
//! - **Only the new events are encoded.** When the stored file is the candidate's own first
//!   lines, header included, the session's carried encoding says so and gives the receipt's
//!   digests without re-encoding what is stored. The new file is the stored one cloned —
//!   copy-on-write where the filesystem has it, an in-kernel copy where it does not — with the
//!   new lines appended, then synced and renamed exactly as a whole write is. When the header
//!   changes (managed context enabled, or a typed user message moving the body to version 4), or
//!   nothing is stored, the stored events are checked and the whole session is written.
//!
//! ## Durability
//!
//! A receipt claims [`Durability::ProcessCrash`]: the file is synced and then renamed over the old
//! one, so a process crash leaves either the old file or the new one. It does not claim
//! [`Durability::PowerLoss`], which would need the parent directory synced after the rename and,
//! on macOS, `F_FULLFSYNC` rather than `fsync` — neither of which this store does for a session.

use std::path::Path;
use std::time::SystemTime;

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
            .checkpoint_identity(id)
            .await
            .map_err(|_| refused(ErrorCode::CheckpointNotCommitted))?;
        if stored != *view.expected {
            return Err(refused(ErrorCode::StaleBase));
        }
        let appendable = self.check_extends(candidate, &stored).await?;
        let revision = accepted_revision(candidate)?;
        let write = Write::of(candidate, &stored, appendable)?;
        let count = u64::try_from(candidate.event_count())
            .map_err(|_| refused(ErrorCode::StorageCapacity))?;
        self.check_artifacts(candidate, &stored, view.artifacts)
            .await?;
        self.commit_write(id, &write, view.expected).await?;
        let receipt = CheckpointReceipt {
            frontier: ContextFrontier {
                session_id: id.as_str().to_owned(),
                event_count: count,
                prefix_blake3: candidate.prefix_digest(count).map_err(|_| unknown())?,
                projection_revision: revision,
            },
            body_digest: candidate.body_digest(),
            durability: Durability::ProcessCrash,
        };
        // Postcondition: a whole write's bytes are the candidate's own prefix at its full count,
        // which is what lets the host derive the next expected identity from the receipt.
        if let Write::Whole(body) = &write {
            assert!(
                Digest::of(body.as_bytes()) == receipt.frontier.prefix_blake3,
                "a checkpoint's frontier digest is the candidate's whole-file digest"
            );
        }
        self.remember(id, &receipt, candidate.body_version()).await;
        Ok(receipt)
    }

    /// The stored identity a checkpoint compares against: the one this process wrote, while the
    /// file still looks as it did after that write, and otherwise the one read from disk.
    async fn checkpoint_identity(&self, id: &SessionId) -> StoreResult<ExpectedCheckpoint> {
        let path = self.session_file(id)?;
        let remembered = self.written_entry(id);
        if let (Some(written), Ok(metadata)) = (remembered, fs::metadata(&path).await)
            && written.stamp == Stamp::of(&metadata)
        {
            return Ok(written.identity);
        }
        self.forget_written(id);
        self.stored_identity_blocking(id).await
    }

    /// Records what a commit left on disk, or forgets it when the file cannot be described.
    async fn remember(&self, id: &SessionId, receipt: &CheckpointReceipt, body_version: u32) {
        let Ok(path) = self.session_file(id) else {
            self.forget_written(id);
            return;
        };
        let Ok(metadata) = fs::metadata(&path).await else {
            self.forget_written(id);
            return;
        };
        let written = Written {
            identity: ExpectedCheckpoint::after(receipt, body_version),
            stamp: Stamp::of(&metadata),
        };
        self.written
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), written);
    }

    /// The identity remembered for `id`, if any.
    fn written_entry(&self, id: &SessionId) -> Option<Written> {
        self.written
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    /// Forgets the identity remembered for `id`: something other than a checkpoint touched it.
    pub(super) fn forget_written(&self, id: &SessionId) {
        self.written
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }

    /// Refuses a candidate that does not extend what is stored: the log is append-only, so a
    /// checkpoint may add events after the stored ones and never change or drop one.
    ///
    /// When the candidate's prefix digest is the stored file's, the file is the candidate's own
    /// first lines, header included, and the new lines can simply follow them: `Ok(true)`. A
    /// header the candidate states differently — the upgrade to managed context, or the move to
    /// version 4 when a typed user message arrives, which may change the header without changing
    /// its version — cannot be appended to, so the stored events are read back and compared with
    /// the candidate's first events, and the whole session is written: `Ok(false)`.
    async fn check_extends(
        &self,
        candidate: &Session,
        stored: &ExpectedCheckpoint,
    ) -> Result<bool, CheckpointError> {
        let ExpectedCheckpoint::Stored {
            file_blake3,
            event_count,
            ..
        } = stored
        else {
            return Ok(false);
        };
        let held = u64::try_from(candidate.event_count()).unwrap_or(u64::MAX);
        if held < *event_count {
            return Err(refused(ErrorCode::StaleBase));
        }
        let prefix = candidate
            .prefix_digest(*event_count)
            .map_err(|_| refused(ErrorCode::StaleBase))?;
        if prefix == *file_blake3 {
            return Ok(true);
        }
        let on_disk = self
            .load_blocking(candidate.id())
            .await
            .map_err(|_| refused(ErrorCode::StaleBase))?;
        let count = on_disk.event_count();
        let same = candidate
            .log()
            .events()
            .get(..count)
            .is_some_and(|events| events == on_disk.log().events());
        if same {
            Ok(false)
        } else {
            Err(refused(ErrorCode::StaleBase))
        }
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

    /// Writes the new file beside the session file, syncs it, and renames it over the real name.
    async fn commit_write(
        &self,
        id: &SessionId,
        write: &Write,
        expected: &ExpectedCheckpoint,
    ) -> Result<(), CheckpointError> {
        let not_committed = refused(ErrorCode::CheckpointNotCommitted);
        let path = self.session_file(id).map_err(|_| not_committed)?;
        let dir = self.session_dir(id).map_err(|_| not_committed)?;
        fs::create_dir_all(&dir).await.map_err(|_| not_committed)?;
        let temp = temp_path(&path);
        let staged = match write {
            Write::Whole(body) => write_synced(&temp, body).await,
            Write::Tail(tail) => extend_synced(&path, &temp, tail).await,
        };
        if let Err(error) = staged {
            tracing::warn!(%error, "a checkpoint's temporary file could not be written");
            discard(&temp).await;
            return Err(not_committed);
        }
        if let Err(source) = fs::rename(&temp, &path).await {
            discard(&temp).await;
            self.forget_written(id);
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
            self.forget_written(id);
            self.undo_resurrection(id);
            return Err(unknown());
        }
        Ok(())
    }
}

/// What a checkpoint writes: the whole session, or the lines after what is stored.
enum Write {
    /// Every line, header first: nothing is stored, or the header changes.
    Whole(String),
    /// The event lines after the stored ones, added to a clone of the stored file.
    Tail(String),
}

impl Write {
    /// Encodes what `candidate` adds to `stored`, refusing what the store may not write.
    ///
    /// `appendable` is what [`JsonlStore::check_extends`] found: the stored file is the
    /// candidate's own first lines, header and all.
    fn of(
        candidate: &Session,
        stored: &ExpectedCheckpoint,
        appendable: bool,
    ) -> Result<Self, CheckpointError> {
        let encoded = match stored {
            ExpectedCheckpoint::Stored { event_count, .. } if appendable => {
                candidate.encoded_lines_from(*event_count).map(Self::Tail)
            }
            _ => candidate.try_to_jsonl().map(Self::Whole),
        };
        encoded.map_err(|error| refused(encoding_refusal(&error)))
    }
}

/// The identity this process last committed for a session, and how its file looked after.
#[derive(Clone, Debug)]
pub(super) struct Written {
    /// What the next checkpoint expects to replace.
    identity: ExpectedCheckpoint,
    /// The file's description right after the commit.
    stamp: Stamp,
}

/// What a file looks like from outside: enough to tell that it is no longer the one written.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    /// Its length in bytes.
    len: u64,
    /// Its last modification, where the platform reports one.
    modified: Option<SystemTime>,
    /// Its inode and device on Unix, which a rename over it always changes.
    inode: Option<(u64, u64)>,
}

impl Stamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt as _;
            Some((metadata.ino(), metadata.dev()))
        };
        #[cfg(not(unix))]
        let inode = None;
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            inode,
        }
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
        file_blake3: Digest::of(raw),
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
///
/// `flush` is not optional. Tokio's `write_all` hands the last buffer to a background write and
/// returns; only `flush` waits for that write and reports its error. Without it a full disk on
/// the final chunk is never seen, `sync_all` succeeds on the short file, and the rename installs
/// a truncated session that a receipt then calls committed.
async fn write_synced(path: &Path, body: &str) -> std::io::Result<()> {
    let mut file = fs::File::create(path).await?;
    file.write_all(body.as_bytes()).await?;
    file.flush().await?;
    file.sync_all().await
}

/// Clones the stored file to `temp` — copy-on-write where the filesystem can — then appends
/// `tail` and syncs the result.
async fn extend_synced(stored: &Path, temp: &Path, tail: &str) -> std::io::Result<()> {
    fs::copy(stored, temp).await?;
    let mut file = fs::OpenOptions::new().append(true).open(temp).await?;
    file.write_all(tail.as_bytes()).await?;
    file.flush().await?;
    file.sync_all().await
}

/// Removes a temporary file, quietly: it was never under the real name.
async fn discard(path: &Path) {
    if fs::remove_file(path).await.is_ok() {
        tracing::debug!(temp = %path.display(), "discarded a checkpoint's temporary file");
    }
}
