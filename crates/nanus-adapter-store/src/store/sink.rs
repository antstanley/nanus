//! One stream's capture sink: exact bytes into a staging file, then a truthful receipt.
//!
//! A sink is `Send` because the shell's pipe drains are spawned tasks; it holds no `Rc` and no
//! store handle, only its own file, its digests, and a share of its call's [`LeaseGuard`].
//!
//! ## What a receipt says
//!
//! - **Complete** only for end of file with every observed byte retained, synced and renamed —
//!   including an empty stream, whose object is empty and whose digest is the empty digest.
//! - **Partial** for any other reason, or when the per-stream cap stopped retention.
//! - **Unavailable** — no id, no digest, nothing retained — when nothing was retained of a
//!   stream that had bytes, or when any write, sync or rename failed or passed its deadline.
//!   An uncertain object stays where it is as an unreferenced, still-charged orphan for garbage
//!   collection; it is never published.
//!
//! Digests are computed as bytes are retained, so a 64 KiB chunk digest and the whole digest
//! exist at finalize without reading the object back.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nanus_domain::SessionId;
use nanus_domain::context::managed::{
    ArtifactId, ArtifactReceipt, CaptureReason, CaptureStatus, CaptureStream, Digest, Hasher,
    RawEncoding, limits,
};
use nanus_ports::artifact::SendBoxFuture;
use nanus_ports::{CaptureFailure, CaptureFinalization, FinalizedArtifact, RawCaptureSink};
use tokio::io::AsyncWriteExt as _;

/// A call's quota reservation, as a marker file locked for as long as anything holds it.
///
/// The lease and both of its sinks share one guard, so the reservation stands until the last of
/// them is gone — a sink still writing after its lease was dropped is still inside its allowance.
/// The lock is what tells garbage collection the lease is alive; the file's removal is what
/// returns the unused allowance.
#[derive(Debug)]
pub struct LeaseGuard {
    /// The marker.
    path: PathBuf,
    /// The open, locked marker; dropping it releases the lock.
    file: Option<std::fs::File>,
}

impl LeaseGuard {
    /// Wraps a marker this process has created and locked.
    pub const fn new(path: PathBuf, file: std::fs::File) -> Self {
        Self {
            path,
            file: Some(file),
        }
    }

    /// Locks the marker. It is fresh, so nobody else can hold it; the lock is for others to see.
    pub fn lock(&self) -> std::io::Result<()> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| std::io::Error::other("no marker"))?;
        file.try_lock().map_err(std::io::Error::from)
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        // Removed while still locked, so a collector that opens it in between sees a live lease.
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => tracing::warn!(%source, "a capture lease marker could not be removed"),
        }
        drop(self.file.take());
    }
}

/// Where one sink writes, and what it calls its object.
#[derive(Debug)]
pub struct SinkPlan {
    /// The session the object belongs to.
    pub session: SessionId,
    /// The call whose output it is.
    pub call_id: String,
    /// Which stream.
    pub stream: CaptureStream,
    /// The object's id.
    pub id: ArtifactId,
    /// The staging file, created by the reservation.
    pub partial: PathBuf,
    /// The finalized object's name.
    pub object: PathBuf,
    /// Bytes this stream may retain.
    pub cap: u64,
    /// Bytes staged in memory before a write.
    pub staging: usize,
    /// How long one write, sync or rename may take.
    pub deadline: Duration,
}

/// One stream's sink.
pub struct FileSink {
    plan: SinkPlan,
    file: Option<tokio::fs::File>,
    retained: u64,
    capped: bool,
    staging: Vec<u8>,
    whole: Hasher,
    chunk: Hasher,
    chunk_fill: u64,
    chunks: Vec<Digest>,
    failure: Option<CaptureFailure>,
    /// Whether the staging file can be removed on drop: nothing physical was ever attempted.
    disposable: bool,
    _lease: Arc<LeaseGuard>,
}

impl core::fmt::Debug for FileSink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FileSink")
            .field("id", &self.plan.id)
            .field("retained", &self.retained)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

impl FileSink {
    /// Builds a sink over a staging file the reservation created.
    pub fn new(plan: SinkPlan, file: std::fs::File, lease: Arc<LeaseGuard>) -> Self {
        assert!(plan.staging > 0, "a sink stages at least one byte");
        assert!(plan.cap <= limits::CAPTURE_STREAM_BYTES_MAX);
        Self {
            staging: Vec::with_capacity(plan.staging),
            plan,
            file: Some(tokio::fs::File::from_std(file)),
            retained: 0,
            capped: false,
            whole: Hasher::new(),
            chunk: Hasher::new(),
            chunk_fill: 0,
            chunks: Vec::new(),
            failure: None,
            disposable: true,
            _lease: lease,
        }
    }

    /// Accepts bytes: retains up to the cap, hashing as it goes, staging and writing in bounds.
    async fn accept(&mut self, bytes: &[u8]) -> Result<(), CaptureFailure> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        let room = self.plan.cap.saturating_sub(self.retained);
        let take = usize::try_from(room).unwrap_or(usize::MAX).min(bytes.len());
        if take < bytes.len() {
            // Beyond the cap the stream keeps draining and the sink stops retaining: the
            // receipt will say partial, and the observed count is the pump's to report.
            self.capped = true;
        }
        let mut rest = bytes.get(..take).unwrap_or_default();
        self.absorb(rest);
        while !rest.is_empty() {
            let space = self.plan.staging.saturating_sub(self.staging.len());
            let (now, later) = rest.split_at_checked(space).unwrap_or((rest, &[]));
            self.staging.extend_from_slice(now);
            rest = later;
            if self.staging.len() >= self.plan.staging {
                self.flush().await?;
            }
        }
        Ok(())
    }

    /// Feeds retained bytes to the whole digest and the chunk digests.
    fn absorb(&mut self, bytes: &[u8]) {
        self.retained = self
            .retained
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        self.whole.update(bytes);
        let mut rest = bytes;
        while !rest.is_empty() {
            let space = limits::ARTIFACT_CHUNK_BYTES_U64.saturating_sub(self.chunk_fill);
            let space = usize::try_from(space).unwrap_or(usize::MAX);
            let (now, later) = rest.split_at_checked(space).unwrap_or((rest, &[]));
            self.chunk.update(now);
            self.chunk_fill = self
                .chunk_fill
                .saturating_add(u64::try_from(now.len()).unwrap_or(u64::MAX));
            if self.chunk_fill == limits::ARTIFACT_CHUNK_BYTES_U64 {
                self.chunks.push(std::mem::take(&mut self.chunk).finish());
                self.chunk_fill = 0;
            }
            rest = later;
        }
        assert!(
            self.retained <= self.plan.cap,
            "retention never passes the cap"
        );
    }

    /// Writes the staged bytes, within the deadline.
    ///
    /// A write that passes its deadline is not abandoned: the file stays owned by the sink, and
    /// the sink is marked failed, so its object can only become an orphan.
    async fn flush(&mut self) -> Result<(), CaptureFailure> {
        if self.staging.is_empty() {
            return Ok(());
        }
        let Some(file) = self.file.as_mut() else {
            return self.fail(CaptureFailure::Io(String::from("the sink has no file")));
        };
        self.disposable = false;
        match tokio::time::timeout(self.plan.deadline, file.write_all(&self.staging)).await {
            Ok(Ok(())) => {
                self.staging.clear();
                Ok(())
            }
            Ok(Err(source)) => self.fail(CaptureFailure::Io(source.to_string())),
            Err(_) => self.fail(CaptureFailure::Timeout),
        }
    }

    /// Records a failure; every later write answers with it.
    fn fail(&mut self, failure: CaptureFailure) -> Result<(), CaptureFailure> {
        self.failure = Some(failure.clone());
        Err(failure)
    }

    /// Finalizes the object and writes the receipt that describes it.
    async fn finish(mut self, observed: u64, reason: CaptureReason) -> CaptureFinalization {
        if self.failure.is_none() && self.flush().await.is_err() {
            tracing::debug!(id = %self.plan.id, "a capture's last write failed");
        }
        if let Some(failure) = self.failure.clone() {
            self.quiesce().await;
            return self.unavailable(observed, failure.reason());
        }
        let empty_at_eof = reason == CaptureReason::Eof && observed == 0 && !self.capped;
        if self.retained == 0 && !empty_at_eof {
            // Nothing to keep: the staging file is removed when the sink drops.
            return self.unavailable(observed, reason);
        }
        if let Err(failure) = self.seal().await {
            return self.unavailable(observed, failure.reason());
        }
        let receipt = self.receipt(observed, reason);
        let artifact = FinalizedArtifact::new(self.plan.session.clone(), receipt.clone());
        CaptureFinalization {
            receipt,
            artifact: Some(artifact),
        }
    }

    /// Syncs the staging file and renames it to the object's name, each within the deadline.
    async fn seal(&mut self) -> Result<(), CaptureFailure> {
        let Some(file) = self.file.take() else {
            return Err(CaptureFailure::Io(String::from("the sink has no file")));
        };
        self.disposable = false;
        let deadline = self.plan.deadline;
        match tokio::time::timeout(deadline, file.sync_all()).await {
            Ok(Ok(())) => {}
            Ok(Err(source)) => return Err(CaptureFailure::Io(source.to_string())),
            Err(_) => return Err(CaptureFailure::Timeout),
        }
        drop(file);
        let rename = tokio::fs::rename(&self.plan.partial, &self.plan.object);
        match tokio::time::timeout(deadline, rename).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(source)) => Err(CaptureFailure::Io(source.to_string())),
            // The rename may still land; the object it makes is unreferenced and charged, and
            // garbage collection removes it.
            Err(_) => Err(CaptureFailure::Timeout),
        }
    }

    /// Waits, within the deadline, for an in-flight write to finish before the sink lets go.
    async fn quiesce(&mut self) {
        if let Some(file) = self.file.as_mut()
            && tokio::time::timeout(self.plan.deadline, file.flush())
                .await
                .is_err()
        {
            tracing::warn!(id = %self.plan.id, "a capture write is still in flight; orphaned");
        }
    }

    /// Builds the receipt of a finalized object.
    fn receipt(&mut self, observed: u64, reason: CaptureReason) -> ArtifactReceipt {
        if self.chunk_fill > 0 {
            self.chunks.push(std::mem::take(&mut self.chunk).finish());
            self.chunk_fill = 0;
        }
        let complete = reason == CaptureReason::Eof && !self.capped && observed == self.retained;
        let (status, reason) = match (complete, reason) {
            (true, _) => (CaptureStatus::Complete, CaptureReason::Eof),
            (false, CaptureReason::Eof) if self.capped => {
                (CaptureStatus::Partial, CaptureReason::Quota)
            }
            // End of file with bytes the sink never saw: something between the pipe and the
            // sink dropped them, which is the archive's write failing to keep up.
            (false, CaptureReason::Eof) => (CaptureStatus::Partial, CaptureReason::WriteError),
            (false, other) => (CaptureStatus::Partial, other),
        };
        let receipt = ArtifactReceipt {
            artifact_id: Some(self.plan.id.clone()),
            call_id: self.plan.call_id.clone(),
            stream: self.plan.stream,
            retained_bytes: self.retained,
            observed_bytes: observed.max(self.retained),
            retained_sha256: Some(std::mem::take(&mut self.whole).finish()),
            status,
            reason,
            encoding: RawEncoding::Raw,
            chunk_sha256: std::mem::take(&mut self.chunks),
        };
        assert!(
            receipt.validate().is_ok(),
            "a finalized receipt is truthful"
        );
        receipt
    }

    /// Builds the receipt of a stream nothing usable was kept of.
    fn unavailable(&self, observed: u64, reason: CaptureReason) -> CaptureFinalization {
        let receipt = ArtifactReceipt {
            artifact_id: None,
            call_id: self.plan.call_id.clone(),
            stream: self.plan.stream,
            retained_bytes: 0,
            observed_bytes: observed,
            retained_sha256: None,
            status: CaptureStatus::Unavailable,
            reason,
            encoding: RawEncoding::Raw,
            chunk_sha256: Vec::new(),
        };
        assert!(
            receipt.validate().is_ok(),
            "an unavailable receipt is truthful"
        );
        CaptureFinalization {
            receipt,
            artifact: None,
        }
    }
}

impl Drop for FileSink {
    fn drop(&mut self) {
        // A staging file nothing was ever written to is not an orphan worth charging for. One
        // that was written to stays, because its write may still be in flight.
        if self.disposable {
            remove_quietly(&self.plan.partial);
        }
    }
}

impl RawCaptureSink for FileSink {
    fn write<'a>(&'a mut self, bytes: &'a [u8]) -> SendBoxFuture<'a, Result<(), CaptureFailure>> {
        Box::pin(self.accept(bytes))
    }

    fn finalize(
        self: Box<Self>,
        observed_bytes: u64,
        reason: CaptureReason,
    ) -> SendBoxFuture<'static, CaptureFinalization> {
        Box::pin((*self).finish(observed_bytes, reason))
    }
}

/// Removes a file, ignoring that it is already gone.
pub fn remove_quietly(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => tracing::warn!(%source, path = %path.display(), "could not remove"),
    }
}
