//! Archive garbage collection: one idle session at a time, against its authoritative log.
//!
//! What a session's log publishes is what the session owns. Collection therefore claims the
//! session — refusing one anybody holds, this process included, because a holder may have an
//! object finalized and not yet published — loads and validates its log, and only then removes
//! what no `artifact/published` receipt names and no live lease covers. A log that does not load
//! prevents collection for that session: an archive is never swept against a log nobody can read.
//!
//! There is no eviction of referenced objects: a referenced object is kept even when the quota is
//! exhausted, and a new reservation is what gets refused.
//!
//! Lock order is the store's: the session claim first, then the quota lock.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use nanus_domain::{SessionEvent, SessionId};
use nanus_ports::{StoreError, StoreResult};

use super::archive::{
    ARTIFACTS_DIR, LEASE_EXT, LeaseRecord, PARTIAL_EXT, RAW_EXT, quota_lock, split_name,
};
use super::{JsonlStore, io_error, locked, not_found, refuse_symlinked_dir, take_claim};

/// What one collection did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Objects and orphans removed.
    pub removed_objects: u64,
    /// Bytes those held, now returned to the quota.
    pub reclaimed_bytes: u64,
    /// Objects kept because the log references them.
    pub kept_objects: u64,
    /// Leases still held by a live capture, whose objects were left alone.
    pub live_leases: u64,
    /// Leases whose holder is gone, removed.
    pub dead_leases: u64,
}

impl JsonlStore {
    /// Removes the archived objects one idle session no longer references.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Locked`] when any claim holds the session — this process's
    /// included — [`StoreError::Retired`] for a deleted one, [`StoreError::NotFound`] for one
    /// that is not stored, [`StoreError::Corrupt`] when its log does not validate (which
    /// prevents collection), and [`StoreError::Io`] when the archive cannot be swept.
    pub async fn collect_garbage(&self, id: &SessionId) -> StoreResult<GcReport> {
        self.refuse_retired(id)?;
        let dir = self.session_dir(id)?;
        refuse_symlinked_dir(&dir).await?;
        if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
            return Err(not_found(id));
        }
        let path = self.lock_file(id)?;
        if self.holds_claim(id) {
            return Err(locked(id, &path));
        }
        let Some(claim) = take_claim(&path)? else {
            return Err(locked(id, &path));
        };
        let outcome = self.collect_claimed(id, dir.join(ARTIFACTS_DIR)).await;
        drop(claim);
        outcome
    }

    /// Collects a session this call has claimed.
    async fn collect_claimed(&self, id: &SessionId, artifacts: PathBuf) -> StoreResult<GcReport> {
        let session = self.load_blocking(id).await?;
        let referenced: BTreeSet<String> = session
            .log()
            .events()
            .iter()
            .filter_map(|event| match event {
                SessionEvent::ArtifactPublished { payload } => payload
                    .artifact_id
                    .as_ref()
                    .map(|artifact| artifact.uuid().to_owned()),
                _ => None,
            })
            .collect();
        refuse_symlinked_dir(&artifacts).await?;
        let home = self.home.clone();
        tokio::task::spawn_blocking(move || {
            let lock = quota_lock(&home).map_err(|failure| StoreError::Io {
                path: home.clone(),
                message: format!("the archive quota lock: {failure}"),
            })?;
            let report = sweep(&artifacts, &referenced);
            drop(lock);
            report
        })
        .await
        .map_err(|error| StoreError::Io {
            path: self.home.clone(),
            message: error.to_string(),
        })?
    }
}

/// Removes every object in `artifacts` that is neither referenced nor under a live lease.
fn sweep(artifacts: &Path, referenced: &BTreeSet<String>) -> StoreResult<GcReport> {
    let mut report = GcReport::default();
    let entries = match std::fs::read_dir(artifacts) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(source) => return Err(io_error(artifacts, &source)),
    };
    let mut objects: Vec<(PathBuf, String, String)> = Vec::new();
    let mut protected: BTreeSet<String> = BTreeSet::new();
    for entry in entries {
        let path = entry.map_err(|source| io_error(artifacts, &source))?.path();
        let Some((stem, ext)) = split_name(&path) else {
            continue;
        };
        if ext == LEASE_EXT {
            match live_lease(&path, &stem) {
                Some(covered) => {
                    report.live_leases = report.live_leases.saturating_add(1);
                    protected.extend(covered);
                }
                None => report.dead_leases = report.dead_leases.saturating_add(1),
            }
        } else if ext == RAW_EXT || ext == PARTIAL_EXT {
            objects.push((path, stem, ext));
        }
    }
    for (path, stem, ext) in objects {
        if protected.contains(&stem) || (ext == RAW_EXT && referenced.contains(&stem)) {
            report.kept_objects = report.kept_objects.saturating_add(1);
            continue;
        }
        let size = std::fs::symlink_metadata(&path).map_or(0, |metadata| metadata.len());
        // `remove_file` unlinks a link rather than its target, so a planted link costs itself.
        match std::fs::remove_file(&path) {
            Ok(()) => {
                report.removed_objects = report.removed_objects.saturating_add(1);
                report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(size);
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(&path, &source)),
        }
    }
    Ok(report)
}

/// Returns the objects a lease covers when its holder is alive; removes it when it is not.
///
/// A holder keeps its marker locked for as long as the lease or either sink exists, so a lock
/// this call can take is a holder that is gone — a crashed process, whose lock the kernel
/// released as it exited.
fn live_lease(path: &Path, stem: &str) -> Option<Vec<String>> {
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    else {
        // A marker that cannot be opened cannot be shown dead, so it is kept as live.
        return Some(LeaseRecord::parse(stem).objects);
    };
    match file.try_lock() {
        // A lock that cannot be examined is treated as held: collection errs toward keeping.
        Err(std::fs::TryLockError::WouldBlock | std::fs::TryLockError::Error(_)) => {
            Some(LeaseRecord::parse(stem).objects)
        }
        Ok(()) => {
            if let Err(source) = std::fs::remove_file(path) {
                tracing::debug!(%source, "a dead lease marker could not be removed");
            }
            drop(file);
            None
        }
    }
}
