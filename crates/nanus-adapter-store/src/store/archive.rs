//! The shell archive: bounded raw captures beside a session, under a cross-process quota.
//!
//! ## Layout
//!
//! ```text
//! <session>/artifacts/<uuid>.partial   a stream being captured, or an orphan
//! <session>/artifacts/<uuid>.raw       a finalized object: exactly the retained bytes
//! <session>/artifacts/<bytes>_<uuid>_<uuid>.lease
//!                                      a reservation, locked by the process that holds it
//! <home>/archive.lock                  the quota lock
//! ```
//!
//! The `<uuid>` is host-generated (UUID v7) and is the [`ArtifactId`] without its `a:` prefix.
//! An id arriving from a receipt is parsed by the domain before it is ever joined to a path, so
//! it is one component of hex digits and hyphens: model input never names a file.
//!
//! ## Quota
//!
//! Usage is not a counter that could drift from the disk: it is *measured*, under the quota lock,
//! by walking every session's archive and every deleted session still in the trash. A lease
//! charges its whole reservation, or what its objects actually hold if that is more; every other
//! object — finalized, partial, orphaned — charges its size. A reservation is refused when it
//! would take a session past [`ArchiveQuota::session_bytes`] or the store past
//! [`ArchiveQuota::store_bytes`]. Referenced objects are never evicted to make room.
//!
//! The quota lock is an operating-system file lock with a short bounded retry, taken off the
//! runtime thread. It is only ever taken with the session's claim already held, never the other
//! way round.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nanus_domain::context::managed::{
    ArtifactId, ArtifactReceipt, CaptureStream, Digest, Hasher, limits,
};
use nanus_domain::{SessionId, ToolCallId};
use nanus_ports::{
    ArtifactError, ArtifactStore, CaptureFailure, CaptureLease, CaptureLimits, FinalizedArtifact,
    LocalBoxFuture,
};

use super::JsonlStore;
use super::sink::{FileSink, LeaseGuard, SinkPlan, remove_quietly};

/// The directory, inside a session's, that holds its archive.
pub const ARTIFACTS_DIR: &str = "artifacts";

/// The extension of a finalized object.
pub const RAW_EXT: &str = "raw";

/// The extension of a staging file or an orphan.
pub const PARTIAL_EXT: &str = "partial";

/// The extension of a reservation marker.
pub const LEASE_EXT: &str = "lease";

/// The file, under the home, whose lock serializes quota decisions between processes.
const QUOTA_LOCK_FILE: &str = "archive.lock";

/// How many times the quota lock is tried before a reservation gives up.
const QUOTA_LOCK_ATTEMPTS: u32 = 50;

/// The pause between two tries of the quota lock.
const QUOTA_LOCK_PAUSE: Duration = Duration::from_millis(10);

/// The archive's byte limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchiveQuota {
    /// Archived bytes one session may hold, live reservations and orphans included.
    pub session_bytes: u64,
    /// Archived bytes the whole store may hold, deleted sessions not yet removed included.
    pub store_bytes: u64,
}

impl Default for ArchiveQuota {
    fn default() -> Self {
        Self {
            session_bytes: limits::CAPTURE_SESSION_BYTES_MAX,
            store_bytes: limits::CAPTURE_STORE_BYTES_MAX,
        }
    }
}

impl ArchiveQuota {
    /// Returns this quota with neither limit above the policy's.
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            session_bytes: self.session_bytes.min(limits::CAPTURE_SESSION_BYTES_MAX),
            store_bytes: self.store_bytes.min(limits::CAPTURE_STORE_BYTES_MAX),
        }
    }
}

/// What a reservation marker says, read from its *name*.
///
/// The name rather than the body, because the marker is locked for as long as its lease lives,
/// and on Windows a locked file cannot be read through another handle: a collector or a quota
/// scan in another process must learn what a live lease covers without opening it. The name is
/// `<reserved bytes>_<uuid>_<uuid>.lease`, written once by the `create_new` that makes it.
#[derive(Debug, PartialEq, Eq)]
pub struct LeaseRecord {
    /// The bytes reserved.
    pub reserved: u64,
    /// The object uuids the reservation covers.
    pub objects: Vec<String>,
}

impl LeaseRecord {
    /// Renders the marker's stem.
    fn stem(&self) -> String {
        let mut stem = self.reserved.to_string();
        for object in &self.objects {
            stem.push('_');
            stem.push_str(object);
        }
        stem
    }

    /// Parses a marker's stem. One that does not parse charges a whole call's maximum and
    /// covers nothing, so it can only ever over-count.
    pub fn parse(stem: &str) -> Self {
        let mut parts = stem.split('_');
        let reserved = parts.next().and_then(|raw| raw.parse::<u64>().ok());
        let objects: Vec<String> = parts
            .filter(|uuid| ArtifactId::parse(&format!("a:{uuid}")).is_some())
            .map(str::to_owned)
            .collect();
        match reserved {
            Some(reserved) if objects.len() == 2 => Self { reserved, objects },
            _ => Self {
                reserved: limits::CAPTURE_STREAM_BYTES_MAX.saturating_mul(2),
                objects: Vec::new(),
            },
        }
    }
}

/// Everything one reservation needs, owned, so it can run off the runtime thread.
struct Reservation {
    home: PathBuf,
    roots: [PathBuf; 2],
    artifacts: PathBuf,
    quota: ArchiveQuota,
    stream_bytes: u64,
}

/// What a granted reservation made.
struct Reserved {
    lease: Arc<LeaseGuard>,
    sinks: [(ArtifactId, std::fs::File); 2],
}

impl JsonlStore {
    /// Returns one session's archive directory.
    fn artifacts_dir(&self, session: &SessionId) -> Option<PathBuf> {
        self.session_dir(session)
            .ok()
            .map(|dir| dir.join(ARTIFACTS_DIR))
    }

    /// Reserves quota and builds two sinks; see [`ArtifactStore::reserve_capture`].
    async fn reserve(
        &self,
        session: &SessionId,
        call_id: &ToolCallId,
        limits: CaptureLimits,
    ) -> Result<CaptureLease, CaptureFailure> {
        let refuse = |why: &str| Err(CaptureFailure::Io(why.to_owned()));
        if self.retired(session).unwrap_or(true) {
            return refuse("the session was deleted");
        }
        if !self.holds_claim(session) {
            return refuse("the session is not held by this process");
        }
        if call_id.as_str().is_empty() || call_id.as_str().len() > 256 {
            return refuse("the call id cannot name a receipt");
        }
        let Some(artifacts) = self.artifacts_dir(session) else {
            return refuse("the session id cannot name a directory");
        };
        prepare_dir(&artifacts).map_err(|source| CaptureFailure::Io(source.to_string()))?;
        let stream_bytes = limits.stream_bytes.min(limits::CAPTURE_STREAM_BYTES_MAX);
        let reservation = Reservation {
            home: self.home.clone(),
            roots: [self.sessions_root(), self.trash_root()],
            artifacts: artifacts.clone(),
            quota: self.quota,
            stream_bytes,
        };
        let reserved = tokio::task::spawn_blocking(move || reservation.grant())
            .await
            .map_err(|error| CaptureFailure::Io(error.to_string()))??;
        // A quarter of the call's staging budget per sink: the capturing shell queues up to half
        // of it in front of the sinks, so the two sinks' buffers take the other half and the
        // call as a whole stays within one budget.
        let staging = limits
            .staging_bytes
            .min(limits::CAPTURE_STAGING_BYTES_MAX)
            .checked_div(4)
            .unwrap_or(0)
            .max(1);
        let [stdout, stderr] = reserved.sinks;
        let sink = |(id, file): (ArtifactId, std::fs::File), stream: CaptureStream| {
            let plan = SinkPlan {
                session: session.clone(),
                call_id: call_id.as_str().to_owned(),
                stream,
                partial: artifacts.join(format!("{}.{PARTIAL_EXT}", id.uuid())),
                object: artifacts.join(format!("{}.{RAW_EXT}", id.uuid())),
                id,
                cap: stream_bytes,
                staging,
                deadline: limits.deadline,
            };
            Box::new(FileSink::new(plan, file, Arc::clone(&reserved.lease)))
        };
        let stdout = sink(stdout, CaptureStream::Stdout);
        let stderr = sink(stderr, CaptureStream::Stderr);
        let lease = reserved.lease;
        Ok(CaptureLease::new(
            call_id.clone(),
            stdout,
            stderr,
            Box::new(move || drop(lease)),
        ))
    }

    /// Resolves an object's path, refusing anything that is not this session's own file.
    fn object_path(&self, session: &SessionId, id: &ArtifactId) -> Result<PathBuf, ArtifactError> {
        let dir = self
            .session_dir(session)
            .map_err(|_| ArtifactError::Denied)?;
        check_dir(&dir)?;
        let artifacts = dir.join(ARTIFACTS_DIR);
        check_dir(&artifacts)?;
        let path = artifacts.join(format!("{}.{RAW_EXT}", id.uuid()));
        assert!(
            path.parent() == Some(artifacts.as_path()),
            "an artifact id names one file in its session's archive"
        );
        Ok(path)
    }

    /// Opens an object, telling a missing one from another session's.
    fn open_object(
        &self,
        session: &SessionId,
        id: &ArtifactId,
    ) -> Result<std::fs::File, ArtifactError> {
        // A session with no archive at all is the same question as an archive without the
        // object, so the foreign check covers both.
        match self
            .object_path(session, id)
            .and_then(|path| open_no_follow(&path))
        {
            Err(ArtifactError::Unavailable) if self.held_elsewhere(session, id) => {
                Err(ArtifactError::Denied)
            }
            other => other,
        }
    }

    /// Whether another session's archive holds an object with this id.
    ///
    /// Asked only when this session's does not, so a reader handed another conversation's
    /// receipt is told it is not theirs rather than that it is missing.
    fn held_elsewhere(&self, session: &SessionId, id: &ArtifactId) -> bool {
        let Ok(own) = self.session_dir(session) else {
            return false;
        };
        let Ok(entries) = std::fs::read_dir(self.sessions_root()) else {
            return false;
        };
        entries.filter_map(Result::ok).any(|entry| {
            let dir = entry.path();
            dir != own
                && [RAW_EXT, PARTIAL_EXT].iter().any(|ext| {
                    let path = dir.join(ARTIFACTS_DIR).join(format!("{}.{ext}", id.uuid()));
                    std::fs::symlink_metadata(path).is_ok()
                })
        })
    }

    /// Checks a finalized object against its receipt: presence, length, every digest.
    pub(super) async fn verify_artifact(
        &self,
        artifact: &FinalizedArtifact,
    ) -> Result<(), ArtifactError> {
        let receipt = artifact.receipt().clone();
        receipt.validate().map_err(|_| ArtifactError::Corrupt)?;
        let Some(id) = receipt.artifact_id.clone() else {
            return Err(ArtifactError::Unavailable);
        };
        let file = self.open_object(artifact.session(), &id)?;
        tokio::task::spawn_blocking(move || verify_whole(file, &receipt))
            .await
            .map_err(|_| ArtifactError::Unavailable)?
    }

    /// Reads a verified range; see [`ArtifactStore::read_range`].
    async fn read_verified(
        &self,
        session: &SessionId,
        receipt: &ArtifactReceipt,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, ArtifactError> {
        receipt.validate().map_err(|_| ArtifactError::Corrupt)?;
        let work = u64::try_from(limits::RECALL_WORK_BYTES).unwrap_or(u64::MAX);
        if length > work {
            return Err(ArtifactError::Denied);
        }
        let Some(id) = receipt.artifact_id.clone() else {
            return Err(ArtifactError::Unavailable);
        };
        let file = self.open_object(session, &id)?;
        let receipt = receipt.clone();
        tokio::task::spawn_blocking(move || read_range(file, &receipt, offset, length))
            .await
            .map_err(|_| ArtifactError::Unavailable)?
    }
}

impl ArtifactStore for JsonlStore {
    fn reserve_capture<'a>(
        &'a self,
        session: &'a SessionId,
        call_id: &'a ToolCallId,
        limits: CaptureLimits,
    ) -> LocalBoxFuture<'a, Result<CaptureLease, CaptureFailure>> {
        Box::pin(self.reserve(session, call_id, limits))
    }

    fn read_range<'a>(
        &'a self,
        session: &'a SessionId,
        receipt: &'a ArtifactReceipt,
        offset: u64,
        length: u64,
    ) -> LocalBoxFuture<'a, Result<Vec<u8>, ArtifactError>> {
        Box::pin(self.read_verified(session, receipt, offset, length))
    }

    fn verify<'a>(
        &'a self,
        artifact: &'a FinalizedArtifact,
    ) -> LocalBoxFuture<'a, Result<(), ArtifactError>> {
        Box::pin(self.verify_artifact(artifact))
    }
}

impl Reservation {
    /// Takes the quota lock, measures, and creates the marker and both staging files.
    fn grant(self) -> Result<Reserved, CaptureFailure> {
        let io = |source: std::io::Error| CaptureFailure::Io(source.to_string());
        let lock = quota_lock(&self.home)?;
        let amount = self.stream_bytes.saturating_mul(2);
        let session = dir_usage(&self.artifacts).map_err(io)?;
        let mut store: u64 = 0;
        for root in &self.roots {
            store = store.saturating_add(root_usage(root).map_err(io)?);
        }
        if session.saturating_add(amount) > self.quota.session_bytes
            || store.saturating_add(amount) > self.quota.store_bytes
        {
            return Err(CaptureFailure::Quota);
        }
        let ids = [new_artifact_id()?, new_artifact_id()?];
        let lease = self.mark(amount, &ids).map_err(io)?;
        let [first, second] = ids;
        let stdout = create_partial(&self.artifacts, &first).map_err(io)?;
        let stderr = match create_partial(&self.artifacts, &second) {
            Ok(file) => file,
            Err(source) => {
                remove_quietly(
                    &self
                        .artifacts
                        .join(format!("{}.{PARTIAL_EXT}", first.uuid())),
                );
                return Err(io(source));
            }
        };
        drop(lock);
        Ok(Reserved {
            lease,
            sinks: [(first, stdout), (second, stderr)],
        })
    }

    /// Creates and locks the reservation's marker.
    fn mark(&self, amount: u64, ids: &[ArtifactId; 2]) -> std::io::Result<Arc<LeaseGuard>> {
        let record = LeaseRecord {
            reserved: amount,
            objects: ids.iter().map(|id| id.uuid().to_owned()).collect(),
        };
        let path = self
            .artifacts
            .join(format!("{}.{LEASE_EXT}", record.stem()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        // The guard owns the marker from its creation, so a failure below removes it.
        let guard = LeaseGuard::new(path, file);
        guard.lock()?;
        Ok(Arc::new(guard))
    }
}

/// Generates a fresh object id.
fn new_artifact_id() -> Result<ArtifactId, CaptureFailure> {
    ArtifactId::parse(&format!("a:{}", uuid::Uuid::now_v7()))
        .ok_or_else(|| CaptureFailure::Io(String::from("a generated id did not parse")))
}

/// Creates a staging file, refusing to reuse a name.
fn create_partial(artifacts: &Path, id: &ArtifactId) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(artifacts.join(format!("{}.{PARTIAL_EXT}", id.uuid())))
}

/// Takes the quota lock, retrying briefly; the returned file is the lock.
pub fn quota_lock(home: &Path) -> Result<std::fs::File, CaptureFailure> {
    let path = home.join(QUOTA_LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| CaptureFailure::Io(source.to_string()))?;
    for _ in 0..QUOTA_LOCK_ATTEMPTS {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(QUOTA_LOCK_PAUSE),
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(CaptureFailure::Io(source.to_string()));
            }
        }
    }
    Err(CaptureFailure::Timeout)
}

/// Creates an archive directory, refusing one that is a link or not a directory.
fn prepare_dir(dir: &Path) -> std::io::Result<()> {
    if let Some(parent) = dir.parent() {
        refuse_link(parent)?;
    }
    std::fs::create_dir_all(dir)?;
    refuse_link(dir)
}

/// Refuses a path that is a symlink or not a directory.
fn refuse_link(dir: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(std::io::Error::other(
            "an archive directory that is a link or a file is never written through",
        )),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(source),
    }
}

/// Checks a directory on the way to an object: absent is unavailable, a link is denied.
fn check_dir(dir: &Path) -> Result<(), ArtifactError> {
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(ArtifactError::Denied),
        Err(_) => Err(ArtifactError::Unavailable),
    }
}

/// Opens an object without following a link at its last component.
fn open_no_follow(path: &Path) -> Result<std::fs::File, ArtifactError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Err(ArtifactError::Denied),
        Err(_) => return Err(ArtifactError::Unavailable),
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // Closes the window between the check above and the open: a link swapped in is refused
        // by the kernel rather than followed.
        options.custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits());
    }
    let file = options.open(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            ArtifactError::Unavailable
        } else {
            ArtifactError::Denied
        }
    })?;
    let regular = file.metadata().is_ok_and(|metadata| metadata.is_file());
    if regular {
        Ok(file)
    } else {
        Err(ArtifactError::Denied)
    }
}

/// Checks an object's length, every chunk digest and the whole digest.
fn verify_whole(mut file: std::fs::File, receipt: &ArtifactReceipt) -> Result<(), ArtifactError> {
    check_length(&file, receipt)?;
    let mut whole = Hasher::new();
    let mut buffer = vec![0_u8; limits::ARTIFACT_CHUNK_BYTES];
    let mut remaining = receipt.retained_bytes;
    for expected in &receipt.chunk_blake3 {
        let size = remaining.min(limits::ARTIFACT_CHUNK_BYTES_U64);
        let chunk = read_chunk(&mut file, &mut buffer, size)?;
        whole.update(chunk);
        if Digest::of(chunk) != *expected {
            return Err(ArtifactError::Corrupt);
        }
        remaining = remaining.saturating_sub(size);
    }
    if remaining != 0 || receipt.retained_blake3.as_ref() != Some(&whole.finish()) {
        return Err(ArtifactError::Corrupt);
    }
    Ok(())
}

/// Reads `[offset, offset + length)`, verifying every chunk it intersects first.
fn read_range(
    mut file: std::fs::File,
    receipt: &ArtifactReceipt,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, ArtifactError> {
    check_length(&file, receipt)?;
    let size = limits::ARTIFACT_CHUNK_BYTES_U64;
    let end = offset.saturating_add(length).min(receipt.retained_bytes);
    if offset >= end {
        return Ok(Vec::new());
    }
    let first = offset.checked_div(size).unwrap_or(0);
    let last = end.saturating_sub(1).checked_div(size).unwrap_or(0);
    let span = last
        .saturating_sub(first)
        .saturating_add(1)
        .saturating_mul(size);
    // The integrity work is whole chunks, so it is the chunks — not the requested length — that
    // the per-call work bound counts.
    if span > u64::try_from(limits::RECALL_WORK_BYTES).unwrap_or(u64::MAX) {
        return Err(ArtifactError::Denied);
    }
    let start = first.saturating_mul(size);
    file.seek(SeekFrom::Start(start))
        .map_err(|_| ArtifactError::Unavailable)?;
    let mut buffer = vec![0_u8; limits::ARTIFACT_CHUNK_BYTES];
    let mut out = Vec::new();
    for index in first..=last {
        let chunk_start = index.saturating_mul(size);
        let chunk_size = receipt.retained_bytes.saturating_sub(chunk_start).min(size);
        let chunk = read_chunk(&mut file, &mut buffer, chunk_size)?;
        let expected = usize::try_from(index)
            .ok()
            .and_then(|index| receipt.chunk_blake3.get(index))
            .ok_or(ArtifactError::Corrupt)?;
        if Digest::of(chunk) != *expected {
            return Err(ArtifactError::Corrupt);
        }
        let from = offset.saturating_sub(chunk_start);
        let to = end.saturating_sub(chunk_start).min(chunk_size);
        let from = usize::try_from(from).map_err(|_| ArtifactError::Corrupt)?;
        let to = usize::try_from(to).map_err(|_| ArtifactError::Corrupt)?;
        out.extend_from_slice(chunk.get(from..to).ok_or(ArtifactError::Corrupt)?);
    }
    assert_eq!(
        u64::try_from(out.len()).ok(),
        Some(end.saturating_sub(offset)),
        "a range read returns exactly the bytes asked for, clamped to the object"
    );
    Ok(out)
}

/// Refuses an object whose length is not the receipt's.
fn check_length(file: &std::fs::File, receipt: &ArtifactReceipt) -> Result<(), ArtifactError> {
    let length = file
        .metadata()
        .map_err(|_| ArtifactError::Unavailable)?
        .len();
    if length == receipt.retained_bytes {
        Ok(())
    } else {
        Err(ArtifactError::Corrupt)
    }
}

/// Reads exactly `size` bytes into the front of `buffer`.
fn read_chunk<'b>(
    file: &mut std::fs::File,
    buffer: &'b mut [u8],
    size: u64,
) -> Result<&'b [u8], ArtifactError> {
    let size = usize::try_from(size).map_err(|_| ArtifactError::Corrupt)?;
    let chunk = buffer.get_mut(..size).ok_or(ArtifactError::Corrupt)?;
    file.read_exact(chunk).map_err(|_| ArtifactError::Corrupt)?;
    Ok(chunk)
}

/// Measures every session archive under one root: `sessions/` or the trash.
fn root_usage(root: &Path) -> std::io::Result<u64> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(source),
    };
    let mut total: u64 = 0;
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            total = total.saturating_add(dir_usage(&entry.path().join(ARTIFACTS_DIR))?);
        }
    }
    Ok(total)
}

/// Measures one archive: each lease at the larger of its reservation and its objects, and
/// every other file at its size.
pub fn dir_usage(artifacts: &Path) -> std::io::Result<u64> {
    let entries = match std::fs::read_dir(artifacts) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(source),
    };
    let mut leases: Vec<LeaseRecord> = Vec::new();
    let mut sizes: BTreeMap<String, u64> = BTreeMap::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some((stem, ext)) = split_name(&path) else {
            continue;
        };
        let size = std::fs::symlink_metadata(&path).map_or(0, |metadata| metadata.len());
        if ext == LEASE_EXT {
            leases.push(LeaseRecord::parse(&stem));
        } else {
            let held = sizes.entry(stem).or_insert(0);
            *held = held.saturating_add(size);
        }
    }
    let mut total: u64 = 0;
    let mut covered: BTreeSet<String> = BTreeSet::new();
    for lease in &leases {
        let actual = lease.objects.iter().fold(0_u64, |sum, object| {
            sum.saturating_add(sizes.get(object).copied().unwrap_or(0))
        });
        total = total.saturating_add(lease.reserved.max(actual));
        covered.extend(lease.objects.iter().cloned());
    }
    for (stem, size) in sizes {
        if !covered.contains(&stem) {
            total = total.saturating_add(size);
        }
    }
    Ok(total)
}

/// Splits an archive file name into its stem and extension.
pub fn split_name(path: &Path) -> Option<(String, String)> {
    let name = path.file_name()?.to_str()?;
    let (stem, ext) = name.rsplit_once('.')?;
    Some((stem.to_owned(), ext.to_owned()))
}
