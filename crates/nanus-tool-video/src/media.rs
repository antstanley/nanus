//! The extension-local contracts between the tool and what feeds it.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use nanus_ports::LocalBoxFuture;

use crate::VideoError;

/// A local copy of the source, hashed once, retained through physical decoder completion.
///
/// Probing and decoding both read this exact copy, so the digest in the manifest names the
/// bytes that were decoded even when the original changes underneath the call.
/// The host/stock owner cleans up only after the snapshot and all retained workers release it.
pub struct Snapshot {
    /// Where the copy lives; the file name is fixed so no model text reaches a command line.
    pub path: PathBuf,
    /// The hex SHA-256 of the copied bytes.
    pub sha256: String,
    /// The copied length.
    pub byte_len: u64,
    /// Opaque host/stock lease; its final release owns cleanup, including physical workers.
    pub(crate) owner: Arc<dyn std::any::Any + Send + Sync>,
}

impl Snapshot {
    /// Retains a caller-owned immutable file without reading, copying or decoding it.
    ///
    /// The host must verify the file's authority, identity, digest and length before construction.
    /// `owner` must retain that exact file and perform cleanup only after its final release.
    /// The absolute path must name a fixed `source` file, never an unchecked model argument.
    /// This constructor checks receipt shape/size only; it grants no filesystem authority.
    ///
    /// # Errors
    ///
    /// Refuses a non-absolute/non-fixed path, malformed SHA-256 or over-bound claimed length.
    /// Refusal releases this owner reference; other physical owners remain responsible for cleanup.
    pub fn from_owned_file(
        path: PathBuf,
        sha256: String,
        byte_len: u64,
        owner: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Result<Self, VideoError> {
        if !path.is_absolute()
            || path.file_name() != Some(std::ffi::OsStr::new("source"))
            || path.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )
            })
            || sha256.len() != 64
            || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            || byte_len > crate::source::SNAPSHOT_BYTES_MAX
        {
            return Err(VideoError::Source(
                "read_video: invalid owned snapshot receipt".to_owned(),
            ));
        }
        assert!(path.is_absolute());
        assert!(byte_len <= crate::source::SNAPSHOT_BYTES_MAX);
        Ok(Self {
            path,
            sha256,
            byte_len,
            owner,
        })
    }

    /// Retains only lifetime ownership for a physical decoder worker, without new source I/O.
    ///
    /// A dropped call/future is not physical process completion. Workers retain this lease until
    /// their process/reader teardown has joined; the host defines the actual cleanup policy.
    #[must_use]
    pub fn retain_owner(&self) -> Arc<dyn std::any::Any + Send + Sync> {
        Arc::clone(&self.owner)
    }
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Snapshot")
            .field("path", &self.path)
            .field("sha256", &self.sha256)
            .field("byte_len", &self.byte_len)
            .finish_non_exhaustive()
    }
}

/// What probing learned about a source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaInfo {
    /// The demuxer name.
    pub container: String,
    /// The selected stream's codec.
    pub video_codec: String,
    /// The selected stream's index.
    pub video_stream: u32,
    /// The source duration.
    pub duration_ms: u64,
    /// The presentation time the stream starts at, which sampling normalises away.
    pub start_offset_ms: i64,
    /// Coded width.
    pub width: u32,
    /// Coded height.
    pub height: u32,
    /// The FFmpeg build that probed it.
    pub decoder_version: String,
}

/// One sampled still.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampledFrame {
    /// The actual presentation time, relative to the start of the source.
    pub timestamp_ms: u64,
    /// JPEG width.
    pub width: u32,
    /// JPEG height.
    pub height: u32,
    /// The encoded JPEG.
    pub jpeg: Vec<u8>,
}

/// The frames a sampling produced, with what it could not do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Frames in time order, at most the requested count.
    pub frames: Vec<SampledFrame>,
    /// Reduced counts and other limits the model should be told.
    pub warnings: Vec<String>,
}

/// The half-open interval a call inspects, on the source timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// Inclusive start.
    pub start_ms: u64,
    /// Exclusive end.
    pub end_ms: u64,
}

/// Creates a bounded snapshot of a workspace file.
pub trait VideoSource {
    /// Copies `path` under the extension's size ceiling.
    fn snapshot<'a>(&'a self, path: &'a str) -> LocalBoxFuture<'a, Result<Snapshot, VideoError>>;
}

/// Probes and samples a snapshot.
pub trait VideoDecoder {
    /// Reads the container and the selected video stream.
    fn probe<'a>(
        &'a self,
        snapshot: &'a Snapshot,
    ) -> LocalBoxFuture<'a, Result<MediaInfo, VideoError>>;

    /// Samples up to `count` stills from the centres of equal portions of `window`.
    fn sample<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        info: &'a MediaInfo,
        window: Window,
        count: u32,
    ) -> LocalBoxFuture<'a, Result<Sample, VideoError>>;
}

/// Where a call's pixels go.
pub trait VideoRouting {
    /// Whether the main model's exact provider, plan, endpoint and model has verified image input.
    ///
    /// Read at call time, because a model or provider switch changes the answer between calls.
    fn main_model_sees_images(&self) -> bool;

    /// The analysis route on the current provider and plan, or why there is none.
    ///
    /// Asked before any source I/O: a route that cannot be used refuses the call cheaply.
    fn analyzer(&self) -> LocalBoxFuture<'_, Result<Rc<dyn VideoAnalyzer>, VideoError>>;
}

/// A vision model that reads labelled stills and answers in text.
pub trait VideoAnalyzer {
    /// The route's identity, recorded in the manifest.
    fn provenance(&self) -> Provenance;

    /// Refuses, before the source is opened, a call this route could not afford.
    ///
    /// Asked with the call's question and frame count, before any copy or decode, and answered
    /// from a worst case: `frames` stills of [`FRAME_EDGE_MAX`](crate::ffmpeg::FRAME_EDGE_MAX)
    /// pixels a side. It is a cheap early refusal, not the charge: [`VideoAnalyzer::analyze`] still
    /// accounts for the stills that were actually sampled. Defaults to admitting every call.
    ///
    /// # Errors
    ///
    /// Returns [`VideoError::Analysis`] when the route's allowance cannot cover the worst case.
    fn admit(&self, question: &str, frames: u32) -> Result<(), VideoError> {
        let _ = (question, frames);
        Ok(())
    }

    /// Sends exactly these stills, once, with no retry.
    fn analyze<'a>(
        &'a self,
        request: &'a AnalysisRequest,
    ) -> LocalBoxFuture<'a, Result<Analysis, VideoError>>;
}

/// What an analysis sends.
#[derive(Clone, Debug)]
pub struct AnalysisRequest {
    /// The caller's question, or the default one.
    pub question: String,
    /// The source interval, so the answer can cite it.
    pub window: Window,
    /// Stills in time order.
    pub frames: Vec<SampledFrame>,
}

/// An analysis answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Analysis {
    /// The model's text, bounded.
    pub answer: String,
    /// Reported usage, absent when the provider did not report it.
    pub usage: Option<AnalysisUsage>,
}

/// Provider-reported usage; never estimated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnalysisUsage {
    /// Input tokens.
    pub input_tokens: u32,
    /// Output tokens, reasoning included.
    pub output_tokens: u32,
    /// The share of output spent reasoning.
    pub reasoning_tokens: u32,
}

/// Which model answered, and under which wire, for the manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    /// The provider's name.
    pub provider: String,
    /// The plan.
    pub plan: String,
    /// The exact model id.
    pub model: String,
    /// The endpoint's origin only: no path, query or credential.
    pub endpoint_origin: String,
    /// The wire protocol.
    pub protocol: String,
    /// The image profile's version.
    pub profile_version: String,
    /// How the request was bounded.
    pub processing: String,
}
