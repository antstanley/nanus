//! The evidence archive: raw shell output kept before its preview is cut.
//!
//! A shell preview is bounded, so what scrolled past the cap was never in the log, and no amount
//! of history persistence could recover it. Capture fixes that for shell streams only: the exact
//! pipe bytes are written to a bounded, call-scoped sink *before* the preview cap, and the
//! finalized object is published to the session by an [`ArtifactReceipt`] — the authoritative
//! manifest, with a digest per 64 KiB chunk so a range can be verified without rehashing 8 MiB.
//!
//! The shape is set by where the bytes come from. The shell's pipe drains are spawned tasks, so
//! a [`RawCaptureSink`] is `Send` and its futures are too; it never holds an `Rc` or a local-only
//! store handle. The [`ArtifactStore`] itself is local, like every other port, and is reached
//! only through the session's [`crate::context::ContextRuntime`].
//!
//! Capture is best effort once a command is authorized: a quota refusal or a sink failure makes
//! the receipt partial or unavailable, never the command denied or its pipes blocked.

use core::future::Future;
use core::pin::Pin;
use std::time::Duration;

use nanus_domain::context::managed::{ArtifactReceipt, CaptureReason, limits};
use nanus_domain::{SessionId, ToolCallId};

use crate::LocalBoxFuture;
use crate::context::FinalizedArtifact;

/// A boxed future that is `Send`, for work that runs inside a spawned pipe pump.
pub type SendBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A shared, key-addressable archive.
pub type ArtifactHandle = std::rc::Rc<Box<dyn ArtifactStore>>;

/// The bounds one capture reservation takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureLimits {
    /// Bytes one stream may retain.
    pub stream_bytes: u64,
    /// Staged buffer bytes per call, across both streams.
    pub staging_bytes: usize,
    /// How long one sink write or finalize may take.
    pub deadline: Duration,
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            stream_bytes: limits::CAPTURE_STREAM_BYTES_MAX,
            staging_bytes: limits::CAPTURE_STAGING_BYTES_MAX,
            deadline: Duration::from_millis(limits::CAPTURE_DEADLINE_MS),
        }
    }
}

/// Why capture could not proceed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CaptureFailure {
    /// A session or store quota refused the reservation or the write.
    #[error("archive quota exhausted")]
    Quota,
    /// This host cannot capture.
    #[error("capture is not supported here")]
    Unsupported,
    /// A sink write or finalize passed its deadline.
    #[error("archive write passed its deadline")]
    Timeout,
    /// The archive could not be written.
    #[error("archive write failed: {0}")]
    Io(String),
}

impl CaptureFailure {
    /// The receipt reason this failure ends a capture with.
    #[must_use]
    pub const fn reason(&self) -> CaptureReason {
        match self {
            Self::Quota => CaptureReason::Quota,
            Self::Unsupported => CaptureReason::Unsupported,
            Self::Timeout => CaptureReason::Timeout,
            Self::Io(_) => CaptureReason::WriteError,
        }
    }
}

/// What finalizing one sink produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureFinalization {
    /// The truthful receipt: complete, partial or unavailable.
    pub receipt: ArtifactReceipt,
    /// The finalized object, absent when nothing was retained or finalization is uncertain.
    pub artifact: Option<FinalizedArtifact>,
}

/// One stream's call-scoped archive sink.
///
/// `observed_bytes` is what the pump actually read, not what the process may have written after
/// the pump stopped. A deadline disables further writes but never abandons an in-flight physical
/// write: the sink stays owned until it is quiescent, and an uncertain finalize quarantines the
/// object as an unreferenced, still-charged orphan rather than publishing it.
pub trait RawCaptureSink: Send {
    /// Appends exact pipe bytes.
    fn write<'a>(&'a mut self, bytes: &'a [u8]) -> SendBoxFuture<'a, Result<(), CaptureFailure>>;

    /// Finalizes the object: flush, sync and rename before any receipt says it exists.
    fn finalize(
        self: Box<Self>,
        observed_bytes: u64,
        reason: CaptureReason,
    ) -> SendBoxFuture<'static, CaptureFinalization>;
}

/// A reservation of archive quota for one call, owning its two sinks until they are taken.
///
/// Dropping a lease releases only allowance no sink used; bytes a sink finalized stay charged
/// until garbage collection or deletion reclaims them.
pub struct CaptureLease {
    call_id: ToolCallId,
    stdout: Option<Box<dyn RawCaptureSink>>,
    stderr: Option<Box<dyn RawCaptureSink>>,
    release: Option<Box<dyn FnOnce()>>,
}

impl core::fmt::Debug for CaptureLease {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CaptureLease")
            .field("call_id", &self.call_id)
            .field("stdout", &self.stdout.is_some())
            .field("stderr", &self.stderr.is_some())
            .finish_non_exhaustive()
    }
}

impl CaptureLease {
    /// Builds a lease from its two sinks and the release of its unused allowance.
    #[must_use]
    pub fn new(
        call_id: ToolCallId,
        stdout: Box<dyn RawCaptureSink>,
        stderr: Box<dyn RawCaptureSink>,
        release: Box<dyn FnOnce()>,
    ) -> Self {
        Self {
            call_id,
            stdout: Some(stdout),
            stderr: Some(stderr),
            release: Some(release),
        }
    }

    /// The call the lease was taken for.
    #[must_use]
    pub const fn call_id(&self) -> &ToolCallId {
        &self.call_id
    }

    /// Transfers the standard-output sink, once.
    pub fn take_stdout(&mut self) -> Option<Box<dyn RawCaptureSink>> {
        self.stdout.take()
    }

    /// Transfers the standard-error sink, once.
    pub fn take_stderr(&mut self) -> Option<Box<dyn RawCaptureSink>> {
        self.stderr.take()
    }
}

impl Drop for CaptureLease {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// Why an archived range could not be returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ArtifactError {
    /// The object is missing or was never retained.
    #[error("the artifact is unavailable")]
    Unavailable,
    /// The object does not match its receipt.
    #[error("the artifact does not match its receipt")]
    Corrupt,
    /// The id does not belong to this session or escapes its directory.
    #[error("the artifact is not this session's")]
    Denied,
}

/// The archive of one store.
pub trait ArtifactStore {
    /// Reserves quota and creates two sinks for one call.
    ///
    /// Session ownership is held by the caller before this is asked; an implementation takes its
    /// cross-process quota lock inside, and never the other way round.
    fn reserve_capture<'a>(
        &'a self,
        session: &'a SessionId,
        call_id: &'a ToolCallId,
        limits: CaptureLimits,
    ) -> LocalBoxFuture<'a, Result<CaptureLease, CaptureFailure>>;

    /// Reads `[offset, offset + length)` of a published object, verifying its length and every
    /// chunk the range intersects against the receipt before returning a byte.
    fn read_range<'a>(
        &'a self,
        session: &'a SessionId,
        receipt: &'a ArtifactReceipt,
        offset: u64,
        length: u64,
    ) -> LocalBoxFuture<'a, Result<Vec<u8>, ArtifactError>>;

    /// Checks that a finalized object is present and matches its receipt.
    fn verify<'a>(
        &'a self,
        artifact: &'a FinalizedArtifact,
    ) -> LocalBoxFuture<'a, Result<(), ArtifactError>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    struct Null;

    impl RawCaptureSink for Null {
        fn write<'a>(&'a mut self, _: &'a [u8]) -> SendBoxFuture<'a, Result<(), CaptureFailure>> {
            Box::pin(async { Ok(()) })
        }

        fn finalize(
            self: Box<Self>,
            _: u64,
            _: CaptureReason,
        ) -> SendBoxFuture<'static, CaptureFinalization> {
            Box::pin(async {
                CaptureFinalization {
                    receipt: ArtifactReceipt {
                        artifact_id: None,
                        call_id: "c".into(),
                        stream: nanus_domain::context::managed::CaptureStream::Stdout,
                        retained_bytes: 0,
                        observed_bytes: 0,
                        retained_sha256: None,
                        status: nanus_domain::context::managed::CaptureStatus::Unavailable,
                        reason: CaptureReason::Unsupported,
                        encoding: nanus_domain::context::managed::RawEncoding::Raw,
                        chunk_sha256: Vec::new(),
                    },
                    artifact: None,
                }
            })
        }
    }

    #[test]
    fn a_lease_hands_each_sink_out_once_and_releases_on_drop() {
        let released = Rc::new(Cell::new(false));
        let flag = Rc::clone(&released);
        let mut lease = CaptureLease::new(
            ToolCallId::new("c"),
            Box::new(Null),
            Box::new(Null),
            Box::new(move || flag.set(true)),
        );
        assert!(lease.take_stdout().is_some());
        assert!(lease.take_stdout().is_none());
        assert!(lease.take_stderr().is_some());
        assert!(!released.get());
        drop(lease);
        assert!(released.get());
    }

    #[test]
    fn a_failure_names_the_reason_its_receipt_records() {
        assert_eq!(CaptureFailure::Quota.reason(), CaptureReason::Quota);
        assert_eq!(
            CaptureFailure::Io("x".into()).reason(),
            CaptureReason::WriteError
        );
    }
}
