//! Deletion: exclusive ownership, a retired id, and a trash that only ever empties.
//!
//! ## Why deletion retires the id
//!
//! A session is a log a holder rewrites whole on every save, so removing its directory is not
//! enough to remove the conversation: an agent that still holds the old
//! [`nanus_domain::Session`] value saves it again on its next turn, and the directory comes back
//! with everything in it. Deletion therefore records the id as **retired** first, durably, and
//! every write path refuses a retired id with [`nanus_ports::StoreError::Retired`] — `save`,
//! `checkpoint`, `lock` and `name`. A stale handle can no longer resurrect what a person deleted.
//!
//! ## The transaction
//!
//! 1. **Exclusive ownership.** A session another process (or another claim) holds is refused
//!    with [`nanus_ports::StoreError::Locked`]. A session *this* process holds is deleted by its
//!    holder.
//! 2. **Retire.** `<home>/retired/<encoded-id>` is written with fsync, and its directory synced.
//! 3. **Move.** The whole session directory is renamed into `<home>/trash/<encoded-id>.<uuid>`,
//!    one atomic step: no reader ever sees half a session.
//! 4. **Remove.** The trash entry is removed, which is what reclaims archive quota: the quota
//!    counts the trash until its bytes are gone.
//! 5. **Release** the claim.
//!
//! A crash anywhere leaves a state [`JsonlStore::new`] finishes rather than reverses: a marker
//! with its directory still in place is moved to the trash (under the claim, so a live deleter in
//! another process is not raced), and the trash is emptied. Nothing ever moves out of the trash.
//!
//! A write that raced the deletion and landed after the move sees the marker in its own
//! post-write check and removes what it recreated; see `save_blocking`.

use std::path::{Path, PathBuf};

use nanus_domain::SessionId;
use nanus_ports::StoreResult;
use tokio::fs;

use super::{JsonlStore, decode_id, encode_id, io_error, locked, take_claim, write_atomic};

/// The directory, under the home, that holds one marker per deleted id.
const RETIRED_DIR: &str = "retired";

/// The directory, under the home, that deleted sessions pass through on their way out.
const TRASH_DIR: &str = "trash";

/// How a deletion owns the session it is removing.
enum Exclusive {
    /// This process already held the claim; the deleter is the holder.
    Held,
    /// The deletion took the claim itself, and drops it when it is done.
    Taken(std::fs::File),
}

impl JsonlStore {
    /// Returns the directory of retirement markers.
    fn retired_root(&self) -> PathBuf {
        self.home.join(RETIRED_DIR)
    }

    /// Returns the directory deleted sessions are moved into.
    pub(super) fn trash_root(&self) -> PathBuf {
        self.home.join(TRASH_DIR)
    }

    /// Returns the retirement marker of one id.
    fn retired_marker(&self, id: &SessionId) -> StoreResult<PathBuf> {
        Ok(self.retired_root().join(encode_id(id)?))
    }

    /// Whether `id` was deleted.
    ///
    /// A single `lstat`, synchronous on purpose: it is asked from inside the claim's critical
    /// section, which cannot yield.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] when the marker cannot be examined.
    pub(super) fn retired(&self, id: &SessionId) -> StoreResult<bool> {
        let marker = self.retired_marker(id)?;
        match std::fs::symlink_metadata(&marker) {
            Ok(_) => Ok(true),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(io_error(&marker, &source)),
        }
    }

    /// Refuses a write to a retired id.
    pub(super) fn refuse_retired(&self, id: &SessionId) -> StoreResult<()> {
        if self.retired(id)? {
            return Err(super::retired(id));
        }
        Ok(())
    }

    /// Deletes a session under exclusive ownership, retiring its id.
    pub(super) async fn delete_blocking(&self, id: &SessionId) -> StoreResult<()> {
        let dir = self.session_dir(id)?;
        let metadata = match fs::symlink_metadata(&dir).await {
            Ok(metadata) => metadata,
            // The port is explicit: deleting something absent is not an error, because the
            // caller asked for it to be gone and it is. An absent id that was never a session is
            // not retired: there is nothing a stale handle could resurrect.
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(io_error(&dir, &source)),
        };
        if metadata.file_type().is_symlink() {
            // A link planted under the home is unlinked, never followed, so it cannot make this
            // delete an outside tree. It was never this store's session, so there is no claim to
            // take inside it and no conversation to retire.
            return fs::remove_file(&dir)
                .await
                .map_err(|source| io_error(&dir, &source));
        }
        let exclusive = self.exclusive(id)?;
        let outcome = self.retire_and_remove(id, &dir).await;
        match exclusive {
            // The holder deleted its own conversation; the claim ends with it.
            Exclusive::Held => self.release_lock_blocking(id),
            Exclusive::Taken(file) => drop(file),
        }
        outcome
    }

    /// Takes exclusive ownership of a session for deletion.
    fn exclusive(&self, id: &SessionId) -> StoreResult<Exclusive> {
        let path = self.lock_file(id)?;
        let locks = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if locks.contains_key(id) {
            return Ok(Exclusive::Held);
        }
        let taken = take_claim(&path)?;
        drop(locks);
        taken.map_or_else(|| Err(locked(id, &path)), |file| Ok(Exclusive::Taken(file)))
    }

    /// Retires `id`, moves its directory into the trash, and removes it.
    async fn retire_and_remove(&self, id: &SessionId, dir: &Path) -> StoreResult<()> {
        self.write_marker(id).await?;
        let target = match self.move_to_trash(id, dir) {
            Ok(target) => target,
            // A save that raced this deletion saw the marker and moved the directory to the trash
            // itself; the session is gone either way, which is what was asked for.
            Err(_)
                if std::fs::symlink_metadata(dir)
                    .is_err_and(|source| source.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        // The deletion is complete once the directory has moved: the id is retired and the
        // session is out of the sessions directory. A removal that fails here leaves bytes for
        // the next open to finish, which is a cost in disk rather than a conversation that
        // survived, so it is reported rather than returned.
        if let Err(source) = fs::remove_dir_all(&target).await {
            tracing::warn!(%source, path = %target.display(), "a deleted session's bytes remain");
        }
        Ok(())
    }

    /// Writes the retirement marker durably: the file synced, then its directory.
    async fn write_marker(&self, id: &SessionId) -> StoreResult<()> {
        let root = self.retired_root();
        fs::create_dir_all(&root)
            .await
            .map_err(|source| io_error(&root, &source))?;
        let marker = self.retired_marker(id)?;
        write_atomic(&marker, &format!("{}\n", id.as_str())).await?;
        sync_dir(&root).map_err(|source| io_error(&root, &source))
    }

    /// Moves a session directory into the trash under a fresh name, atomically.
    fn move_to_trash(&self, id: &SessionId, dir: &Path) -> StoreResult<PathBuf> {
        let trash = self.trash_root();
        std::fs::create_dir_all(&trash).map_err(|source| io_error(&trash, &source))?;
        let target = trash.join(format!("{}.{}", encode_id(id)?, uuid::Uuid::now_v7()));
        std::fs::rename(dir, &target).map_err(|source| io_error(dir, &source))?;
        if let Err(source) = sync_dir(&self.sessions_root()) {
            tracing::debug!(%source, "the sessions directory could not be synced");
        }
        assert!(
            target.parent() == Some(trash.as_path()),
            "a trash entry is one component under the trash"
        );
        Ok(target)
    }

    /// Removes a session directory a write recreated after its id was retired.
    ///
    /// Best effort: the marker already makes every later write refuse, and what a failure here
    /// leaves is finished by the next open.
    pub(super) fn undo_resurrection(&self, id: &SessionId) {
        let Ok(dir) = self.session_dir(id) else {
            return;
        };
        match std::fs::symlink_metadata(&dir) {
            Ok(metadata) if metadata.is_dir() => match self.move_to_trash(id, &dir) {
                Ok(target) => remove_entry(&target),
                Err(error) => tracing::warn!(%error, "a retired session could not be moved"),
            },
            Ok(_) | Err(_) => {}
        }
    }

    /// Finishes deletions a crash interrupted: retired directories, then the trash.
    pub(super) async fn finish_interrupted_deletions(&self) {
        for id in self.retired_ids().await {
            let Ok(dir) = self.session_dir(&id) else {
                continue;
            };
            let Ok(metadata) = fs::symlink_metadata(&dir).await else {
                continue;
            };
            if !metadata.is_dir() {
                continue;
            }
            // Under the claim, so a deleter that is alive in another process — its marker
            // written, its move not yet made — is left to finish its own work.
            let Ok(path) = self.lock_file(&id) else {
                continue;
            };
            if let Ok(Some(claim)) = take_claim(&path) {
                if let Err(error) = self.move_to_trash(&id, &dir) {
                    tracing::warn!(%error, "a retired session could not be moved");
                }
                drop(claim);
            }
        }
        self.empty_trash().await;
    }

    /// Reads every retired id.
    async fn retired_ids(&self) -> Vec<SessionId> {
        let mut ids = Vec::new();
        let Ok(mut entries) = fs::read_dir(self.retired_root()).await else {
            return ids;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(id) = entry.file_name().to_str().and_then(decode_id) {
                ids.push(id);
            }
        }
        ids
    }

    /// Removes everything in the trash. Nothing is ever moved back out of it.
    async fn empty_trash(&self) {
        let Ok(mut entries) = fs::read_dir(self.trash_root()).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            remove_entry(&entry.path());
        }
    }
}

/// Removes one trash entry without following a link.
fn remove_entry(path: &Path) {
    let removed = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(source) => Err(source),
    };
    if let Err(source) = removed {
        tracing::warn!(%source, path = %path.display(), "a trash entry could not be removed");
    }
}

/// Synchronizes a directory, so an entry created or renamed in it survives a crash.
///
/// On Unix a directory is opened and `fsync`ed like a file. Elsewhere there is no safe portable
/// equivalent, and the file's own sync is what the store relies on.
// The `Result` is the Unix signature; on other platforms the body cannot fail, and the lint
// that notices would have the two platforms disagree on how this is called.
#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}
